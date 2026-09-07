//! Authenticated content access for executable Wheat operations.

use wheat_core::{ContentId, ContentIo};

use crate::{ContentStore, ContentStoreError};

impl ContentIo for ContentStore {
    type Error = ContentStoreError;

    fn resolve(&self, id: ContentId) -> Result<Vec<u8>, Self::Error> {
        self.get(id)
    }

    fn store_generated(&self, id: ContentId, bytes: &[u8]) -> Result<(), Self::Error> {
        let actual = self.put(bytes)?;
        if actual != id {
            return Err(ContentStoreError::Corrupt {
                expected: id,
                actual,
            });
        }
        Ok(())
    }
}
