//! Where a served repository keeps its head and the receipts of recent publishes.
//!
//! The server reads and moves a repository's head through a [`HeadStore`], so an
//! embedder can keep it in its own storage: for example a database row compared and
//! swapped on `generation`, with the receipts in a table keyed by publish key. The
//! standalone server keeps both in one `repository.head` record
//! ([`LocalPublicationStore`]).
//!
//! A receipt is what answers a publish or a write sent again, so how long a store keeps
//! receipts is part of the contract: see [`HeadStore::receipt_limit`]. With the
//! `conformance` feature, `conformance::check_head_store` checks a store against it.

use std::io;

use async_trait::async_trait;
use wheat_core::{MAX_PUBLISH_RECEIPTS, PublishReceiptRecord};
use wheat_store::{LocalPublicationStore, PublicationError, PublicationStore, RepositoryHead};

#[cfg(feature = "conformance")]
pub mod conformance;

/// What a head store holds under a publish key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptLookup {
    /// The receipt recorded under the key.
    Found(PublishReceiptRecord),
    /// No receipt: none was recorded under the key, or it was recorded longer than
    /// [`wheat_store::PUBLISH_RECEIPT_RETENTION_SECONDS`] ago.
    Absent,
    /// No receipt, from a store that may have dropped receipts younger than the
    /// retention to stay within its [`HeadStore::receipt_limit`]: the key's receipt may
    /// have been one of them. Every receipt dropped so far was recorded at or before
    /// `oldest_kept`, seconds since the Unix epoch.
    Trimmed { oldest_kept: u64 },
}

impl ReceiptLookup {
    /// The receipt, if one was found.
    #[must_use]
    pub fn found(&self) -> Option<&PublishReceiptRecord> {
        match self {
            Self::Found(receipt) => Some(receipt),
            Self::Absent | Self::Trimmed { .. } => None,
        }
    }
}

/// A repository's head and the receipts of the publishes that moved it. A backend's own
/// failures (a database being down) are [`PublicationError::Io`].
///
/// A store keeps each receipt [`advance`](Self::advance) records for at least
/// [`wheat_store::PUBLISH_RECEIPT_RETENTION_SECONDS`] after it was recorded, so a
/// publish or a write sent again within that time gets its first answer instead of
/// being applied a second time. It may drop a receipt any time after that. A store
/// whose [`receipt_limit`](Self::receipt_limit) is `Some` may also drop younger ones,
/// oldest first, to keep no more receipts than the limit; it must then answer a key it
/// holds no receipt for with [`ReceiptLookup::Trimmed`] rather than
/// [`ReceiptLookup::Absent`] for as long as it may have dropped that key's receipt
/// early.
#[async_trait]
pub trait HeadStore: Send + Sync {
    /// The most receipts this store keeps, dropping the oldest beyond it however young
    /// they are; `None` when it keeps every receipt for the whole retention.
    fn receipt_limit(&self) -> Option<usize>;

    /// The current head.
    async fn head(&self) -> Result<RepositoryHead, PublicationError>;

    /// The current head, and what the store holds under the publish key `key`, read
    /// together.
    async fn head_with_receipt(
        &self,
        key: &[u8; 16],
    ) -> Result<(RepositoryHead, ReceiptLookup), PublicationError>;

    /// Moves the head from `expected` to `proposed`, recording `receipts` in the same
    /// step, or fails with [`PublicationError::Stale`] when the head is not `expected`.
    /// `now`, seconds since the Unix epoch, is when the receipts are recorded, and what
    /// a store keeping receipts by age measures their age against.
    async fn advance(
        &self,
        expected: RepositoryHead,
        proposed: RepositoryHead,
        receipts: &[PublishReceiptRecord],
        now: u64,
    ) -> Result<(), PublicationError>;
}

// The record is read and written under a file lock, synchronously, so each call runs
// on a thread for blocking work. All its receipts are rewritten with each publish, so
// it keeps no more than `MAX_PUBLISH_RECEIPTS` of them.
#[async_trait]
impl HeadStore for LocalPublicationStore {
    fn receipt_limit(&self) -> Option<usize> {
        Some(MAX_PUBLISH_RECEIPTS)
    }

    async fn head(&self) -> Result<RepositoryHead, PublicationError> {
        blocking(self, |store| PublicationStore::read(&store)).await
    }

    async fn head_with_receipt(
        &self,
        key: &[u8; 16],
    ) -> Result<(RepositoryHead, ReceiptLookup), PublicationError> {
        let (head, receipts) = blocking(self, |store| store.read_with_receipts()).await?;
        Ok((head, local_lookup(&receipts, key)))
    }

    async fn advance(
        &self,
        expected: RepositoryHead,
        proposed: RepositoryHead,
        receipts: &[PublishReceiptRecord],
        now: u64,
    ) -> Result<(), PublicationError> {
        let receipts = receipts.to_vec();
        blocking(self, move |store| {
            store.compare_and_advance_with_receipts(expected, proposed, &receipts, now)
        })
        .await
    }
}

/// What `receipts`, a local head's, oldest first, hold under `key`. A head holding
/// [`MAX_PUBLISH_RECEIPTS`] of them may have dropped younger ones to stay there. It
/// keeps no mark of whether it did, so a full one is taken to have.
fn local_lookup(receipts: &[PublishReceiptRecord], key: &[u8; 16]) -> ReceiptLookup {
    if let Some(receipt) = receipts.iter().find(|receipt| &receipt.key == key) {
        return ReceiptLookup::Found(*receipt);
    }
    match receipts.first() {
        Some(oldest) if receipts.len() >= MAX_PUBLISH_RECEIPTS => ReceiptLookup::Trimmed {
            oldest_kept: oldest.recorded_at,
        },
        _ => ReceiptLookup::Absent,
    }
}

/// Runs `work` on a handle to `store` on a thread for blocking work.
async fn blocking<T: Send + 'static>(
    store: &LocalPublicationStore,
    work: impl FnOnce(LocalPublicationStore) -> Result<T, PublicationError> + Send + 'static,
) -> Result<T, PublicationError> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || work(store))
        .await
        .map_err(|error| PublicationError::Io(io::Error::other(error)))?
}
