//! Bounded LRU cache of downloaded segments backed by temp files.

use std::collections::{HashMap, VecDeque};
use std::io::{Seek, SeekFrom};

use wheat_core::SegmentId;

use crate::packed::segment::{SegmentError, SegmentReader};

/// A cached segment: in-memory index with a temp file for range reads.
struct CacheEntry {
    reader: SegmentReader,
    total_bytes: usize,
}

/// Bounded LRU cache of segment readers keyed by `SegmentId`.
pub(super) struct SegmentCache {
    entries: HashMap<SegmentId, CacheEntry>,
    order: VecDeque<SegmentId>,
    total_bytes: usize,
    max_bytes: usize,
}

impl SegmentCache {
    pub(super) fn new(max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            total_bytes: 0,
            max_bytes,
        }
    }

    /// Look up an object in a cached segment.
    ///
    /// Returns `(kind, payload_bytes)` if the segment is cached and contains
    /// the requested object. Returns `None` if the segment is not cached or
    /// does not contain the object.
    pub(super) fn get(
        &mut self,
        segment_id: SegmentId,
        namespace: u8,
        digest: [u8; 32],
    ) -> Result<Option<(u16, Vec<u8>)>, SegmentError> {
        let Some(entry) = self.entries.get_mut(&segment_id) else {
            return Ok(None);
        };
        // Move to back of LRU order.
        self.order.retain(|id| *id != segment_id);
        self.order.push_back(segment_id);

        entry
            .reader
            .lookup_entry_bounded(namespace, digest, usize::MAX)
            .map(|opt| opt.map(|(index_entry, payload)| (index_entry.kind, payload.into_bytes())))
    }

    /// Insert a segment into the cache from its raw bytes.
    ///
    /// Writes the bytes to a temp file, opens it as a `SegmentReader`
    /// (authenticating the segment), and adds it to the cache. Evicts oldest
    /// entries if the total cached bytes would exceed the budget.
    pub(super) fn insert(
        &mut self,
        segment_id: SegmentId,
        segment_bytes: &[u8],
    ) -> Result<&mut SegmentReader, SegmentError> {
        if self.entries.contains_key(&segment_id) {
            // Already cached — just refresh LRU position.
            self.order.retain(|id| *id != segment_id);
            self.order.push_back(segment_id);
            return Ok(&mut self.entries.get_mut(&segment_id).unwrap().reader);
        }

        // Evict oldest entries to make room.
        while self.total_bytes + segment_bytes.len() > self.max_bytes && !self.order.is_empty() {
            let oldest = self.order.pop_front().unwrap();
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.total_bytes -= evicted.total_bytes;
            }
        }

        // Write to a temp file and open as a SegmentReader.
        let mut tmp = tempfile::tempfile().map_err(SegmentError::Io)?;
        std::io::Write::write_all(&mut tmp, segment_bytes).map_err(SegmentError::Io)?;
        tmp.seek(SeekFrom::Start(0)).map_err(SegmentError::Io)?;

        let reader = SegmentReader::open(tmp)?;
        let total_bytes = segment_bytes.len();
        self.total_bytes += total_bytes;
        self.order.push_back(segment_id);
        self.entries.insert(
            segment_id,
            CacheEntry {
                reader,
                total_bytes,
            },
        );
        Ok(&mut self.entries.get_mut(&segment_id).unwrap().reader)
    }

    /// Returns all cached segment IDs.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn cached_segments(&self) -> Vec<SegmentId> {
        self.order.iter().copied().collect()
    }
}
