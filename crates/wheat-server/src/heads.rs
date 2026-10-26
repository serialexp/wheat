//! Where a served repository keeps its head and the receipts of recent publishes.
//!
//! The server reads and moves a repository's head through a [`HeadStore`], so an
//! embedder can keep it in its own storage: for example a database row compared and
//! swapped on `generation`, with the receipts in a table keyed by publish key. The
//! standalone server keeps both in one `repository.head` record
//! ([`LocalPublicationStore`]).

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

#[async_trait]
impl HeadStore for LocalPublicationStore {
    async fn head(&self) -> Result<RepositoryHead, PublicationError> {
        PublicationStore::read(self)
    }

    async fn head_with_receipt(
        &self,
        key: &[u8; 16],
    ) -> Result<(RepositoryHead, Option<PublishReceiptRecord>), PublicationError> {
        let (head, receipts) = self.read_with_receipts()?;
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
        self.compare_and_advance_with_receipts(expected, proposed, receipts, now)
    }
}
