//! Where a served repository keeps its head and the receipts of recent publishes.
//!
//! The server reads and moves a repository's head through a [`HeadStore`], so an
//! embedder can keep it in its own storage: for example a database row compared and
//! swapped on `generation`, with the receipts in a table keyed by publish key. The
//! standalone server keeps both in one `repository.head` record
//! ([`LocalPublicationStore`]).

use std::io;

use async_trait::async_trait;
use wheat_core::PublishReceiptRecord;
use wheat_store::{LocalPublicationStore, PublicationError, PublicationStore, RepositoryHead};

/// A repository's head and the receipts of the publishes that moved it. A backend's own
/// failures (a database being down) are [`PublicationError::Io`].
#[async_trait]
pub trait HeadStore: Send + Sync {
    /// The current head.
    async fn head(&self) -> Result<RepositoryHead, PublicationError>;

    /// The current head, and the receipt it keeps under the publish key `key`, if any,
    /// read together.
    async fn head_with_receipt(
        &self,
        key: &[u8; 16],
    ) -> Result<(RepositoryHead, Option<PublishReceiptRecord>), PublicationError>;

    /// Moves the head from `expected` to `proposed`, recording `receipts` in the same
    /// step, or fails with [`PublicationError::Stale`] when the head is not `expected`.
    /// A receipt is kept for [`wheat_store::PUBLISH_RECEIPT_RETENTION_SECONDS`] after it
    /// was recorded, by `now`, unless [`wheat_core::MAX_PUBLISH_RECEIPTS`] newer ones
    /// are; a publish sent again after its receipt is gone is published again.
    async fn advance(
        &self,
        expected: RepositoryHead,
        proposed: RepositoryHead,
        receipts: &[PublishReceiptRecord],
        now: u64,
    ) -> Result<(), PublicationError>;
}

// The record is read and written under a file lock, synchronously, so each call runs
// on a thread for blocking work.
#[async_trait]
impl HeadStore for LocalPublicationStore {
    async fn head(&self) -> Result<RepositoryHead, PublicationError> {
        blocking(self, |store| PublicationStore::read(&store)).await
    }

    async fn head_with_receipt(
        &self,
        key: &[u8; 16],
    ) -> Result<(RepositoryHead, Option<PublishReceiptRecord>), PublicationError> {
        let (head, receipts) = blocking(self, |store| store.read_with_receipts()).await?;
        let receipt = receipts.into_iter().find(|receipt| &receipt.key == key);
        Ok((head, receipt))
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
