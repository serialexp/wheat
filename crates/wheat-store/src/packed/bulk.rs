//! One-pass mutable pack construction for isolated deferred imports.
//!
//! Unlike the durable active pack, this writer has no published tail while it
//! is mutable. Payloads are appended exactly once and the V1 index, directory,
//! and footer are emitted only when the pack rolls or the store is consumed.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use wheat_core::{SEGMENT_PREFIX_ENCODED_SIZE, SegmentIndexEntry, encode_segment_prefix};

use super::segment::{
    SegmentError, SegmentMetadata, emit_cumulative_tail, projected_total_length, verify_logical,
};

const PREFIX_LEN: u64 = SEGMENT_PREFIX_ENCODED_SIZE as u64;
const IO_BUFFER_SIZE: usize = 64 * 1024;

pub(crate) struct BulkSegment {
    path: PathBuf,
    file: File,
    entries: Vec<SegmentIndexEntry>,
    entries_by_key: HashMap<(u8, [u8; 32]), usize>,
    payload_bytes: u64,
    io_buffer: Vec<u8>,
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
            payload_bytes: 0,
            io_buffer: vec![0; IO_BUFFER_SIZE],
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn projected_physical_size(
        &self,
        additional_count: usize,
        additional_payload: u64,
    ) -> Result<u64, SegmentError> {
        let count = self
            .entries
            .len()
            .checked_add(additional_count)
            .ok_or(SegmentError::TooManyEntries)?;
        let payload = self
            .payload_bytes
            .checked_add(additional_payload)
            .ok_or(SegmentError::TooLarge)?;
        projected_total_length(count, payload)
    }

    pub(crate) fn append_prepared(
        &mut self,
        prepared: super::segment::PreparedSegmentRecord<'_>,
    ) -> Result<SegmentIndexEntry, SegmentError> {
        let record = prepared.logical;
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
        };
        self.file.seek(SeekFrom::Start(entry.offset))?;
        self.file.write_all(&physical.bytes)?;
        self.payload_bytes += stored_length;
        self.entries_by_key
            .insert((entry.namespace, entry.digest), self.entries.len());
        self.entries.push(entry.clone());
        Ok(entry)
    }

    pub(crate) fn lookup_bounded(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
        max: usize,
    ) -> Result<Option<(u16, Vec<u8>)>, SegmentError> {
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
        let stored_length = usize::try_from(entry.stored_length)
            .map_err(|_| SegmentError::Format("stored length"))?;
        let mut stored = vec![0; stored_length];
        self.file.seek(SeekFrom::Start(entry.offset))?;
        self.file.read_exact(&mut stored)?;
        let codec = crate::codec::Codec::from_byte(entry.codec)?;
        let bytes = crate::codec::decode(codec, stored, entry.logical_length, max)?;
        verify_logical(&entry, &bytes)?;
        Ok(Some((entry.kind, bytes)))
    }

    pub(crate) fn finalize(mut self) -> Result<FinalizedBulk, SegmentError> {
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
