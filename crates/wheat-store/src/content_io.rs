//! Authenticated content access for executable Wheat operations.

use wheat_core::{ContentId, ContentIo, VerifiedContent};

use crate::{ContentStore, ContentStoreError};

impl ContentIo for ContentStore {
    type Error = ContentStoreError;

    fn resolve(&self, id: ContentId) -> Result<VerifiedContent, Self::Error> {
        self.get_verified(id)
    }

    fn store_generated(&self, content: VerifiedContent<&[u8]>) -> Result<(), Self::Error> {
        self.put_verified(content)
    }
}
