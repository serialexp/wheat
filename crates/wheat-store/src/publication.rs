use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::PathBuf,
};

use thiserror::Error;
use wheat_core::{
    MAX_PUBLISH_RECEIPTS, PublishReceiptRecord, RepositoryEnvelopeId, RepositoryHeadRecord,
    StoreRecordError, decode_repository_head, encode_repository_head,
};

use crate::Durability;

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

    fn record(self, receipts: Vec<PublishReceiptRecord>) -> RepositoryHeadRecord {
        RepositoryHeadRecord {
            generation: self.generation,
            envelope: self.envelope,
            receipts,
        }
    }

    fn of(record: &RepositoryHeadRecord) -> Self {
        Self {
            generation: record.generation,
            envelope: record.envelope,
        }
    }
}

/// How long a head keeps a publish receipt: a retry later than this is published
/// again rather than answered from the receipt.
pub const PUBLISH_RECEIPT_RETENTION_SECONDS: u64 = 24 * 60 * 60;

#[derive(Debug, Error)]
pub enum PublicationError {
    #[error("repository head is not initialized")]
    Missing,
    #[error("repository head is corrupt or has an unsupported version")]
    CorruptHead,
    #[error("repository head does not encode")]
    Encode(#[source] StoreRecordError),
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
        lock.lock()?;
        let result = operation();
        let unlock_result = lock.unlock();
        match (result, unlock_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(PublicationError::Io(error)),
        }
    }

    fn read_unlocked(&self) -> Result<RepositoryHead, PublicationError> {
        Ok(RepositoryHead::of(&self.read_record_unlocked()?))
    }

    fn read_record_unlocked(&self) -> Result<RepositoryHeadRecord, PublicationError> {
        let bytes = match fs::read(&self.head_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(PublicationError::Missing);
            }
            Err(error) => return Err(PublicationError::Io(error)),
        };
        decode_repository_head(&bytes).map_err(|_| PublicationError::CorruptHead)
    }

    /// The head and the receipts of the publishes it keeps, oldest first.
    pub fn read_with_receipts(
        &self,
    ) -> Result<(RepositoryHead, Vec<PublishReceiptRecord>), PublicationError> {
        let record = self.with_lock(|| self.read_record_unlocked())?;
        Ok((RepositoryHead::of(&record), record.receipts))
    }

    /// [`PublicationStore::compare_and_advance`] that also records `receipts`, the
    /// publishes `proposed` holds, in the same write. Receipts older than
    /// [`PUBLISH_RECEIPT_RETENTION_SECONDS`] at `now` are dropped, and then the oldest
    /// past [`MAX_PUBLISH_RECEIPTS`].
    pub fn compare_and_advance_with_receipts(
        &self,
        expected: RepositoryHead,
        proposed: RepositoryHead,
        receipts: &[PublishReceiptRecord],
        now: u64,
    ) -> Result<(), PublicationError> {
        self.advance(expected, proposed, |kept| {
            kept.retain(|receipt| {
                now.saturating_sub(receipt.recorded_at) < PUBLISH_RECEIPT_RETENTION_SECONDS
            });
            kept.extend_from_slice(receipts);
            let over = kept.len().saturating_sub(MAX_PUBLISH_RECEIPTS);
            kept.drain(..over);
        })
    }

    /// Moves the head from `expected` to `proposed`, with the receipts `update` makes
    /// of the current ones.
    fn advance(
        &self,
        expected: RepositoryHead,
        proposed: RepositoryHead,
        update: impl FnOnce(&mut Vec<PublishReceiptRecord>),
    ) -> Result<(), PublicationError> {
        let Some(next_generation) = expected.generation.checked_add(1) else {
            return Err(PublicationError::InvalidGeneration);
        };
        if proposed.generation != next_generation {
            return Err(PublicationError::InvalidGeneration);
        }
        self.with_lock(|| {
            let current = self.read_record_unlocked()?;
            let actual = RepositoryHead::of(&current);
            if actual != expected {
                return Err(PublicationError::Stale { expected, actual });
            }
            let mut receipts = current.receipts;
            update(&mut receipts);
            self.write_unlocked(&proposed.record(receipts))
        })
    }

    /// Hold the repository publication lock while checking exact logical authority
    /// and performing a physical-only commit.
    pub(crate) fn with_repack_authority<T>(
        &self,
        expected: RepositoryHead,
        operation: impl FnOnce() -> Result<T, crate::RepackError>,
    ) -> Result<T, crate::RepackError> {
        let parent = self
            .head_path
            .parent()
            .ok_or_else(|| crate::RepackError::Authority("head path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)?;
        lock.lock()?;
        let result = match self.read_unlocked() {
            Ok(actual) if actual == expected => operation(),
            Ok(_) => Err(crate::RepackError::StaleAuthority),
            Err(error) => Err(crate::RepackError::Authority(error.to_string())),
        };
        let unlock_result = lock.unlock();
        match (result, unlock_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(crate::RepackError::Io(error)),
        }
    }

    fn write_unlocked(&self, head: &RepositoryHeadRecord) -> Result<(), PublicationError> {
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
            file.write_all(&encode_repository_head(head).map_err(PublicationError::Encode)?)?;
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
            Err(PublicationError::Missing) => self.write_unlocked(&initial.record(Vec::new())),
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
        // The receipts stay: an advance that records none does not end the retry
        // window of the publishes before it.
        self.advance(expected, proposed, |_| {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn head(generation: u64) -> RepositoryHead {
        RepositoryHead {
            generation,
            envelope: RepositoryEnvelopeId::from_digest([generation as u8; 32]),
        }
    }

    fn receipt(key: u8, recorded_at: u64) -> PublishReceiptRecord {
        PublishReceiptRecord {
            key: [key; 16],
            request: [0; 16],
            receipt: [key; 16],
            generation: 1,
            envelope: head(1).envelope,
            recorded_at,
        }
    }

    fn keys(store: &LocalPublicationStore) -> Vec<u8> {
        let (_, receipts) = store.read_with_receipts().unwrap();
        receipts.iter().map(|receipt| receipt.key[0]).collect()
    }

    fn store() -> (tempfile::TempDir, LocalPublicationStore) {
        let dir = tempfile::tempdir().unwrap();
        let store =
            LocalPublicationStore::with_durability(dir.path().join("head"), Durability::Deferred);
        store.initialize(head(0)).unwrap();
        (dir, store)
    }

    #[test]
    fn receipts_are_written_with_the_head_and_kept_by_a_plain_advance() {
        let (_dir, store) = store();
        store
            .compare_and_advance_with_receipts(head(0), head(1), &[receipt(1, NOW)], NOW)
            .unwrap();
        assert_eq!(store.read().unwrap(), head(1));
        assert_eq!(keys(&store), [1]);
        store.compare_and_advance(head(1), head(2)).unwrap();
        assert_eq!(keys(&store), [1]);
    }

    #[test]
    fn a_stale_advance_records_nothing() {
        let (_dir, store) = store();
        let stale =
            store.compare_and_advance_with_receipts(head(1), head(2), &[receipt(1, NOW)], NOW);
        assert!(matches!(stale, Err(PublicationError::Stale { .. })));
        assert_eq!(keys(&store), Vec::<u8>::new());
    }

    #[test]
    fn receipts_past_the_retention_window_are_dropped_on_the_next_record() {
        let (_dir, store) = store();
        let old = NOW - PUBLISH_RECEIPT_RETENTION_SECONDS;
        store
            .compare_and_advance_with_receipts(
                head(0),
                head(1),
                &[receipt(1, old), receipt(2, old + 1)],
                old + 1,
            )
            .unwrap();
        store
            .compare_and_advance_with_receipts(head(1), head(2), &[receipt(3, NOW)], NOW)
            .unwrap();
        assert_eq!(keys(&store), [2, 3]);
    }

    #[test]
    fn the_oldest_receipts_past_the_limit_are_dropped() {
        let (_dir, store) = store();
        let full: Vec<_> = (0..MAX_PUBLISH_RECEIPTS)
            .map(|i| receipt((i % 200) as u8, NOW))
            .collect();
        store
            .compare_and_advance_with_receipts(head(0), head(1), &full, NOW)
            .unwrap();
        store
            .compare_and_advance_with_receipts(
                head(1),
                head(2),
                &[receipt(201, NOW), receipt(202, NOW)],
                NOW,
            )
            .unwrap();
        let kept = keys(&store);
        assert_eq!(kept.len(), MAX_PUBLISH_RECEIPTS);
        assert_eq!(kept[..2], [2, 3]);
        assert_eq!(kept[kept.len() - 2..], [201, 202]);
    }
}
