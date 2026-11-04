//! Authenticated content access for executable Wheat operations.

use wheat_core::{ContentId, ContentIo, VerifiedContent};

use crate::{ContentStore, ContentStoreError};

impl ContentIo for ContentStore {
    type Error = ContentStoreError;

    fn resolve(&self, id: ContentId) -> Result<VerifiedContent, Self::Error> {
        self.get_verified(id)
    }

    fn confirm(&self, id: ContentId) -> Result<(), Self::Error> {
        ContentStore::confirm(self, id)
    }

    fn known_length(&self, id: ContentId) -> Result<Option<u64>, Self::Error> {
        ContentStore::logical_length(self, id).map(Some)
    }

    fn store_generated(&self, content: VerifiedContent<&[u8]>) -> Result<(), Self::Error> {
        self.put_verified(content)
    }
}
