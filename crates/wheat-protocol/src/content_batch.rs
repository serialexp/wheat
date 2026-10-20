//! Content on the wire, either way: blobs up to [`MAX_CONTENT_CHUNK_SIZE`] packed
//! into `ContentBatch` messages, larger ones as `ContentChunk`s.

use crate::{
    ContentBatchOutput, ContentChunkOutput, ContentEntry, Digest256, MAX_CONTENT_BATCH_ITEMS,
    MAX_CONTENT_BATCH_SIZE, MAX_CONTENT_CHUNK_SIZE, WheatProtocolMessage,
};

/// Gathers blobs into `ContentBatch` messages that stay within
/// [`MAX_CONTENT_BATCH_SIZE`] and [`MAX_CONTENT_BATCH_ITEMS`]. It closes a batch
/// before a blob would push it past either, so no message outgrows the limits.
pub struct ContentBatcher {
    op_id: u32,
    sequence: u32,
    contents: Vec<ContentEntry>,
    bytes: usize,
}

impl ContentBatcher {
    #[must_use]
    pub fn new(op_id: u32) -> Self {
        Self {
            op_id,
            sequence: 0,
            contents: Vec::new(),
            bytes: 0,
        }
    }

    /// The operation the batches belong to.
    #[must_use]
    pub fn op_id(&self) -> u32 {
        self.op_id
    }

    /// Adds blob `id`. Returns the batch it closed to make room, to send now.
    ///
    /// # Panics
    ///
    /// If `data` is over [`MAX_CONTENT_CHUNK_SIZE`]: such a blob goes as
    /// [`content_chunks`].
    pub fn push(&mut self, id: Digest256, data: Vec<u8>) -> Option<WheatProtocolMessage> {
        assert!(
            data.len() <= MAX_CONTENT_CHUNK_SIZE,
            "a blob of {} bytes goes in chunks, not in a batch",
            data.len()
        );
        let full = self.contents.len() == MAX_CONTENT_BATCH_ITEMS
            || self.bytes + data.len() > MAX_CONTENT_BATCH_SIZE;
        let closed = if full { self.finish() } else { None };
        self.bytes += data.len();
        self.contents.push(ContentEntry { id, data });
        closed
    }

    /// Closes the open batch. Returns it, to send now, unless it is empty.
    pub fn finish(&mut self) -> Option<WheatProtocolMessage> {
        if self.contents.is_empty() {
            return None;
        }
        let batch = WheatProtocolMessage::ContentBatch(ContentBatchOutput {
            message_type: 0x31,
            op_id: self.op_id,
            sequence: self.sequence,
            contents: std::mem::take(&mut self.contents),
        });
        self.bytes = 0;
        self.sequence += 1;
        Some(batch)
    }
}

/// Blob `id` as `ContentChunk` messages of at most [`MAX_CONTENT_CHUNK_SIZE`] bytes
/// each, in order.
pub fn content_chunks<'a>(
    op_id: u32,
    id: &'a Digest256,
    data: &'a [u8],
) -> impl Iterator<Item = WheatProtocolMessage> + 'a {
    let mut offset = 0;
    data.chunks(MAX_CONTENT_CHUNK_SIZE).map(move |piece| {
        let start = offset;
        offset += piece.len();
        WheatProtocolMessage::ContentChunk(ContentChunkOutput {
            message_type: 0x32,
            op_id,
            content_id: id.clone(),
            offset: start as u64,
            data: piece.to_vec(),
            is_final: offset == data.len(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MAX_MESSAGE_SIZE, encode_message};

    fn id(n: usize) -> Digest256 {
        let mut bytes = vec![0; 32];
        bytes[..8].copy_from_slice(&(n as u64).to_le_bytes());
        Digest256 { bytes }
    }

    fn contents(message: &WheatProtocolMessage) -> &[ContentEntry] {
        match message {
            WheatProtocolMessage::ContentBatch(batch) => &batch.contents,
            other => panic!("expected ContentBatch, got {other:?}"),
        }
    }

    #[test]
    fn a_batch_closes_before_a_blob_would_take_it_past_the_byte_limit() {
        let mut batcher = ContentBatcher::new(7);
        // Three blobs one byte short of the limit together, then a full chunk:
        // before, the check came after adding, and the fourth blob made a message
        // over 16 MiB.
        let chunk = MAX_CONTENT_CHUNK_SIZE;
        assert_eq!(MAX_CONTENT_BATCH_SIZE, 3 * chunk);
        assert!(batcher.push(id(0), vec![1; chunk]).is_none());
        assert!(batcher.push(id(1), vec![2; chunk]).is_none());
        assert!(batcher.push(id(2), vec![3; chunk - 1]).is_none());
        let closed = batcher
            .push(id(3), vec![4; chunk])
            .expect("the fourth blob does not fit");
        assert_eq!(contents(&closed).len(), 3);
        assert!(encode_message(&closed).unwrap().len() <= MAX_MESSAGE_SIZE);
        let last = batcher.finish().unwrap();
        assert_eq!(contents(&last).len(), 1);
        assert!(batcher.finish().is_none());
    }

    #[test]
    fn a_batch_holds_at_most_the_item_limit() {
        let mut batcher = ContentBatcher::new(7);
        let mut sent = Vec::new();
        for n in 0..MAX_CONTENT_BATCH_ITEMS * 2 + 1 {
            sent.extend(batcher.push(id(n), vec![0; 10]));
        }
        sent.extend(batcher.finish());
        let sizes: Vec<usize> = sent.iter().map(|batch| contents(batch).len()).collect();
        assert_eq!(sizes, [MAX_CONTENT_BATCH_ITEMS, MAX_CONTENT_BATCH_ITEMS, 1]);
        let sequences: Vec<u32> = sent
            .iter()
            .map(|batch| match batch {
                WheatProtocolMessage::ContentBatch(batch) => batch.sequence,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(sequences, [0, 1, 2]);
    }

    #[test]
    #[should_panic(expected = "goes in chunks")]
    fn a_blob_over_a_chunk_is_not_batched() {
        ContentBatcher::new(7).push(id(0), vec![0; MAX_CONTENT_CHUNK_SIZE + 1]);
    }

    #[test]
    fn chunks_cover_the_blob_in_order() {
        let data: Vec<u8> = (0..MAX_CONTENT_CHUNK_SIZE * 2 + 5)
            .map(|n| n as u8)
            .collect();
        let blob = id(0);
        let mut joined = Vec::new();
        let mut finals = Vec::new();
        for chunk in content_chunks(7, &blob, &data) {
            let WheatProtocolMessage::ContentChunk(chunk) = chunk else {
                panic!("expected ContentChunk");
            };
            assert_eq!(chunk.offset, joined.len() as u64);
            assert!(chunk.data.len() <= MAX_CONTENT_CHUNK_SIZE);
            joined.extend_from_slice(&chunk.data);
            finals.push(chunk.is_final);
        }
        assert_eq!(joined, data);
        assert_eq!(finals, [false, false, true]);
    }
}
