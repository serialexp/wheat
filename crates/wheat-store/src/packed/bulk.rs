//! One-pass mutable pack construction for isolated deferred imports.
//!
//! Unlike the durable active pack, this writer has no published tail while it
//! is mutable. Payloads are appended exactly once and the V1 index, directory,
//! and footer are emitted only when the pack rolls or the store is consumed.
//!
//! Small metadata records (below `RAW_THRESHOLD`) are buffered and flushed as
//! compressed blocks (codec 4) at finalization. Larger records are written
//! individually with per-record compression.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use wheat_core::{SEGMENT_PREFIX_ENCODED_SIZE, SegmentIndexEntry, encode_segment_prefix};

use super::segment::{
    BLOCK_TARGET_BYTES, SegmentError, SegmentMetadata, emit_cumulative_tail,
    projected_total_length, verify_logical,
};
use crate::codec::{self, Codec};

const PREFIX_LEN: u64 = SEGMENT_PREFIX_ENCODED_SIZE as u64;
const IO_BUFFER_SIZE: usize = 64 * 1024;

/// A pending block record buffered for block compression at finalization.
struct PendingBulkBlockRecord {
    namespace: u8,
    kind: u16,
    digest: [u8; 32],
    logical_bytes: Vec<u8>,
}

pub(crate) struct BulkSegment {
    path: PathBuf,
    file: File,
    entries: Vec<SegmentIndexEntry>,
    entries_by_key: HashMap<(u8, [u8; 32]), usize>,
    /// Exact within-block offset and aggregate logical length recorded when the
    /// writer flushes a block. This keeps active lookups O(1) in entry count.
    block_layout_by_key: HashMap<(u8, [u8; 32]), (usize, usize)>,
    payload_bytes: u64,
    io_buffer: Vec<u8>,
    /// Small records waiting to be compressed into a block at finalization.
    pending_block: Vec<PendingBulkBlockRecord>,
    /// Sum of logical byte lengths in `pending_block`.
    pending_block_bytes: usize,
}

pub(crate) struct FinalizedBulk {
    pub path: PathBuf,
    pub id: wheat_core::SegmentId,
    pub metadata: SegmentMetadata,
}

impl BulkSegment {
    pub(crate) fn create(path: impl AsRef<Path>) -> Result<Self, SegmentError> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(&encode_segment_prefix()?)?;
        Ok(Self {
            path,
            file,
            entries: Vec::new(),
            entries_by_key: HashMap::new(),
            block_layout_by_key: HashMap::new(),
            payload_bytes: 0,
            io_buffer: vec![0; IO_BUFFER_SIZE],
            pending_block: Vec::new(),
            pending_block_bytes: 0,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.pending_block.is_empty()
    }

    pub(crate) fn projected_physical_size(
        &self,
        additional_count: usize,
        additional_payload: u64,
    ) -> Result<u64, SegmentError> {
        let count = self
            .entries
            .len()
            .checked_add(self.pending_block.len())
            .and_then(|n| n.checked_add(additional_count))
            .ok_or(SegmentError::TooManyEntries)?;
        // Conservatively assume pending block bytes compress to their full size.
        let payload = self
            .payload_bytes
            .checked_add(self.pending_block_bytes as u64)
            .and_then(|n| n.checked_add(additional_payload))
            .ok_or(SegmentError::TooLarge)?;
        // Bulk segments hold whole records only.
        projected_total_length(count, 0, payload)
    }

    pub(crate) fn append_prepared(
        &mut self,
        prepared: super::segment::PreparedSegmentRecord<'_>,
    ) -> Result<SegmentIndexEntry, SegmentError> {
        let record = prepared.logical;

        // Route small metadata records into the pending block buffer.
        if record.namespace == super::segment::METADATA_NAMESPACE
            && record.bytes.len() < codec::RAW_THRESHOLD
        {
            let entry = SegmentIndexEntry {
                namespace: record.namespace,
                kind: record.kind,
                digest: record.digest,
                // Placeholder values — will be updated at flush time.
                offset: 0,
                codec: Codec::ZstdBlock.to_byte(),
                stored_length: 0,
                logical_length: record.bytes.len() as u64,
                delta_base: None,
                delta_depth: 0,
            };
            self.entries_by_key.insert(
                (entry.namespace, entry.digest),
                self.entries.len() + self.pending_block.len(),
            );
            self.pending_block.push(PendingBulkBlockRecord {
                namespace: record.namespace,
                kind: record.kind,
                digest: record.digest,
                logical_bytes: record.bytes.to_vec(),
            });
            self.pending_block_bytes += record.bytes.len();
            if self.pending_block_bytes >= BLOCK_TARGET_BYTES {
                self.flush_pending_block()?;
            }
            return Ok(entry);
        }

        // Large record: write individually.
        let physical = prepared.physical;
        let stored_length = physical.bytes.len() as u64;
        self.projected_physical_size(1, stored_length)?;
        let entry = SegmentIndexEntry {
            namespace: record.namespace,
            kind: record.kind,
            digest: record.digest,
            offset: PREFIX_LEN + self.payload_bytes,
            codec: physical.codec.to_byte(),
            stored_length,
            logical_length: physical.logical_length,
            delta_base: None,
            delta_depth: 0,
        };
        self.file.seek(SeekFrom::Start(entry.offset))?;
        self.file.write_all(&physical.bytes)?;
        self.payload_bytes += stored_length;
        self.entries_by_key
            .insert((entry.namespace, entry.digest), self.entries.len());
        self.entries.push(entry.clone());
        Ok(entry)
    }

    /// Compresses the pending block records into one zstd frame and writes it.
    fn flush_pending_block(&mut self) -> Result<(), SegmentError> {
        if self.pending_block.is_empty() {
            return Ok(());
        }

        // Sort by (namespace, digest) for deterministic block layout.
        self.pending_block
            .sort_unstable_by_key(|r| (r.namespace, r.digest));

        // Concatenate logical bytes.
        let mut concatenated = Vec::with_capacity(self.pending_block_bytes);
        for record in &self.pending_block {
            concatenated.extend_from_slice(&record.logical_bytes);
        }

        // Compress as one zstd frame.
        let compressed = zstd::bulk::compress(&concatenated, codec::ZSTD_LEVEL)
            .map_err(codec::CodecError::Zstd)?;

        let stored_length = compressed.len() as u64;
        let block_offset = PREFIX_LEN + self.payload_bytes;
        self.file.seek(SeekFrom::Start(block_offset))?;
        self.file.write_all(&compressed)?;
        self.payload_bytes += stored_length;

        // Emit entries and retain the already-known writer layout so active
        // lookups never scan the full entry table to rediscover block offsets.
        let block_logical_length = concatenated.len();
        let mut logical_offset = 0usize;
        for record in self.pending_block.drain(..) {
            let key = (record.namespace, record.digest);
            let idx = self.entries.len();
            self.entries_by_key.insert(key, idx);
            self.block_layout_by_key
                .insert(key, (logical_offset, block_logical_length));
            self.entries.push(SegmentIndexEntry {
                namespace: record.namespace,
                kind: record.kind,
                digest: record.digest,
                offset: block_offset,
                codec: Codec::ZstdBlock.to_byte(),
                stored_length,
                logical_length: record.logical_bytes.len() as u64,
                delta_base: None,
                delta_depth: 0,
            });
            logical_offset += record.logical_bytes.len();
        }
        self.pending_block_bytes = 0;
        Ok(())
    }

    pub(crate) fn lookup_bounded(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
        max: usize,
    ) -> Result<Option<(u16, Vec<u8>)>, SegmentError> {
        // Check pending block records first (not yet flushed to file).
        for record in &self.pending_block {
            if record.namespace == namespace && record.digest == digest {
                let logical_length = record.logical_bytes.len();
                if logical_length > max {
                    return Err(SegmentError::ReadLimit {
                        minimum: logical_length,
                    });
                }
                verify_logical(
                    &SegmentIndexEntry {
                        namespace: record.namespace,
                        kind: record.kind,
                        digest: record.digest,
                        offset: 0,
                        codec: Codec::ZstdBlock.to_byte(),
                        stored_length: 0,
                        logical_length: logical_length as u64,
                        delta_base: None,
                        delta_depth: 0,
                    },
                    &record.logical_bytes,
                )?;
                return Ok(Some((record.kind, record.logical_bytes.clone())));
            }
        }

        let Some(&index) = self.entries_by_key.get(&(namespace, digest)) else {
            return Ok(None);
        };
        let entry = self.entries[index].clone();
        let logical_length =
            usize::try_from(entry.logical_length).map_err(|_| SegmentError::ReadLimit {
                minimum: usize::MAX,
            })?;
        if logical_length > max {
            return Err(SegmentError::ReadLimit {
                minimum: logical_length,
            });
        }

        let codec_val = Codec::from_byte(entry.codec)?;
        if codec_val == Codec::ZstdBlock {
            // Block-compressed: decompress the entire block, extract this
            // record by computing its position from co-block entries.
            let stored_length = usize::try_from(entry.stored_length)
                .map_err(|_| SegmentError::Format("stored length"))?;
            let mut stored = vec![0; stored_length];
            self.file.seek(SeekFrom::Start(entry.offset))?;
            self.file.read_exact(&mut stored)?;
            let (offset_in_block, block_logical_length) = self
                .block_layout_by_key
                .get(&(namespace, digest))
                .copied()
                .ok_or(SegmentError::Format("missing active block layout"))?;
            let decompressed = codec::decode_block(&stored, block_logical_length)?;

            let end = offset_in_block
                .checked_add(logical_length)
                .ok_or(SegmentError::Format("block record overflows"))?;
            if end > decompressed.len() {
                return Err(SegmentError::Format(
                    "block record exceeds decompressed block",
                ));
            }
            let bytes = decompressed[offset_in_block..end].to_vec();
            verify_logical(&entry, &bytes)?;
            Ok(Some((entry.kind, bytes)))
        } else {
            let stored_length = usize::try_from(entry.stored_length)
                .map_err(|_| SegmentError::Format("stored length"))?;
            let mut stored = vec![0; stored_length];
            self.file.seek(SeekFrom::Start(entry.offset))?;
            self.file.read_exact(&mut stored)?;
            let bytes = codec::decode(codec_val, stored, entry.logical_length, max)?;
            verify_logical(&entry, &bytes)?;
            Ok(Some((entry.kind, bytes)))
        }
    }

    pub(crate) fn finalize(mut self) -> Result<FinalizedBulk, SegmentError> {
        // Flush any remaining pending block records.
        self.flush_pending_block()?;

        let index_start = PREFIX_LEN
            .checked_add(self.payload_bytes)
            .ok_or(SegmentError::TooLarge)?;
        let footer = emit_cumulative_tail(
            &mut self.file,
            &mut self.entries,
            index_start,
            &mut self.io_buffer,
        )?;
        self.file.flush()?;
        drop(self.file);
        Ok(FinalizedBulk {
            path: self.path,
            id: footer.segment_id(),
            metadata: super::segment::metadata(&footer),
        })
    }
}
