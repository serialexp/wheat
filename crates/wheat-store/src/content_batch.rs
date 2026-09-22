//! Explicit bounded content staging; ordinary content puts remain durable.
use std::collections::BTreeMap;

use wheat_core::ContentId;

use crate::{ContentStore, ContentStoreError};

const MAX_OBJECTS: usize = 1024;
const MAX_BYTES: usize = 8 * 1024 * 1024;

/// A bounded content batch. Buffered IDs must not be published until `commit` succeeds.
/// Full batches are made durable automatically; dropping discards only the pending tail.
pub struct ContentWriteBatch<'a> {
    store: &'a ContentStore,
    pending: BTreeMap<ContentId, Vec<u8>>,
    bytes: usize,
}

impl<'a> ContentWriteBatch<'a> {
    pub fn new(store: &'a ContentStore) -> Self {
        Self {
            store,
            pending: BTreeMap::new(),
            bytes: 0,
        }
    }

    pub fn put(&mut self, bytes: &[u8]) -> Result<ContentId, ContentStoreError> {
        let id = ContentId::of(bytes);
        if self.pending.contains_key(&id) {
            return Ok(id);
        }
        if bytes.len() > MAX_BYTES {
            self.commit()?;
            return self.store.put(bytes);
        }
        if self.pending.len() == MAX_OBJECTS || self.bytes + bytes.len() > MAX_BYTES {
            self.commit()?;
        }
        self.pending.insert(id, bytes.to_vec());
        self.bytes += bytes.len();
        Ok(id)
    }

    /// Store content with an optional delta base hint.
    ///
    /// If `base_id` refers to content already in the store, and a delta
    /// against it is smaller than the raw content, the delta envelope is
    /// stored instead. Otherwise falls back to raw storage.
    pub fn put_with_base(
        &mut self,
        bytes: &[u8],
        base_id: ContentId,
    ) -> Result<ContentId, ContentStoreError> {
        let id = ContentId::of(bytes);
        if self.pending.contains_key(&id) {
            return Ok(id);
        }
        // Flush pending first so the base (if it's pending) is durable.
        self.commit()?;
        self.store.put_with_base(id, bytes, base_id)?;
        Ok(id)
    }

    /// Durably retains the pending tail. Failed batches remain available for retry.
    pub fn commit(&mut self) -> Result<(), ContentStoreError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.store.put_many(
            self.pending.iter().map(|(id, bytes)| (*id, bytes.clone())),
            1,
        )?;
        self.pending.clear();
        self.bytes = 0;
        Ok(())
    }
}
