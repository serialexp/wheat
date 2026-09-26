//! Receiving items (content blobs or objects), whole or in chunks.
//!
//! The client receives content after a `ContentFetchRequest` and objects during a
//! fetch; the server receives content after `ContentNeeded`. The sender is
//! untrusted, so [`TransferReceiver`] refuses items larger than the cap and chunks
//! that do not continue where the last one stopped. A receiver for requested items
//! also refuses anything that was not asked for, or that arrived twice. It only
//! tracks identities; hashing the finished bytes is the caller's job.

use std::collections::HashSet;
use std::fmt;
use std::hash::Hash;

/// Collects the items of one request, or any items the sender pushes.
///
/// An item arrives either whole or as chunks with `offset` equal to the bytes
/// received so far. Chunks of one item must not be interleaved with chunks of
/// another, though whole items may arrive in between.
#[derive(Debug)]
pub struct TransferReceiver<K> {
    /// `None` accepts any item (the sender pushes unasked).
    wanted: Option<HashSet<K>>,
    partial: Option<(K, Vec<u8>)>,
    max_item_bytes: usize,
}

impl<K: Copy + Eq + Hash + fmt::Debug> TransferReceiver<K> {
    /// Expect each of `wanted` once, none larger than `max_item_bytes`.
    pub fn new(wanted: impl IntoIterator<Item = K>, max_item_bytes: usize) -> Self {
        Self {
            wanted: Some(wanted.into_iter().collect()),
            partial: None,
            max_item_bytes,
        }
    }

    /// Accept any items, none larger than `max_item_bytes`. The caller decides what
    /// to keep; a repeated item is not refused.
    pub fn unrequested(max_item_bytes: usize) -> Self {
        Self {
            wanted: None,
            partial: None,
            max_item_bytes,
        }
    }

    fn take_wanted(&mut self, id: K) -> Result<(), TransferError<K>> {
        if let Some(wanted) = &mut self.wanted
            && !wanted.remove(&id)
        {
            return Err(TransferError::NotRequested(id));
        }
        Ok(())
    }

    /// Accept an item that arrived in one message.
    pub fn accept_whole(&mut self, id: K, len: usize) -> Result<(), TransferError<K>> {
        if len > self.max_item_bytes {
            return Err(TransferError::TooLarge {
                id,
                limit: self.max_item_bytes,
            });
        }
        self.take_wanted(id)
    }

    /// Accept one chunk. Returns the finished item after its final chunk.
    pub fn accept_chunk(
        &mut self,
        id: K,
        offset: u64,
        data: Vec<u8>,
        is_final: bool,
    ) -> Result<Option<(K, Vec<u8>)>, TransferError<K>> {
        let mut buf = match self.partial.take() {
            Some((open, buf)) if open == id => buf,
            Some((open, buf)) => {
                self.partial = Some((open, buf));
                return Err(TransferError::Interleaved { open, received: id });
            }
            None => {
                self.take_wanted(id)?;
                Vec::new()
            }
        };

        if offset != buf.len() as u64 {
            return Err(TransferError::OffsetMismatch {
                id,
                expected: buf.len() as u64,
                received: offset,
            });
        }
        let new_len = match buf.len().checked_add(data.len()) {
            Some(len) if len <= self.max_item_bytes => len,
            _ => {
                return Err(TransferError::TooLarge {
                    id,
                    limit: self.max_item_bytes,
                });
            }
        };

        if buf.is_empty() {
            // The first chunk's buffer becomes the item; a single-chunk item is
            // never copied.
            buf = data;
        } else {
            if new_len > buf.capacity() {
                // Grow by doubling, but never past the cap, so an item at the cap
                // does not hold twice its size.
                let target = buf
                    .capacity()
                    .saturating_mul(2)
                    .max(new_len)
                    .min(self.max_item_bytes);
                buf.reserve_exact(target - buf.len());
            }
            buf.extend_from_slice(&data);
        }

        if is_final {
            Ok(Some((id, buf)))
        } else {
            self.partial = Some((id, buf));
            Ok(None)
        }
    }

    /// True once every requested item has arrived in full (for an unrequested
    /// receiver: once no chunked item is open).
    pub fn is_complete(&self) -> bool {
        self.wanted.as_ref().is_none_or(HashSet::is_empty) && self.partial.is_none()
    }

    /// Requested items not yet received in full, counting one still arriving in
    /// chunks.
    pub fn outstanding(&self) -> usize {
        self.wanted.as_ref().map_or(0, HashSet::len) + usize::from(self.partial.is_some())
    }

    /// Check that nothing is left, when the sender says it is done.
    pub fn finish(self) -> Result<(), TransferError<K>> {
        if let Some((id, _)) = self.partial {
            return Err(TransferError::Unfinished(id));
        }
        match self.wanted {
            Some(wanted) if !wanted.is_empty() => Err(TransferError::Missing(wanted.len())),
            _ => Ok(()),
        }
    }

    /// Check that no chunked item is open, keeping the requested items that have
    /// not arrived: for a sender that may answer part of a request.
    pub fn finish_partial(self) -> Result<HashSet<K>, TransferError<K>> {
        if let Some((id, _)) = self.partial {
            return Err(TransferError::Unfinished(id));
        }
        Ok(self.wanted.unwrap_or_default())
    }
}

/// Why a received item was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum TransferError<K> {
    /// The item was not asked for, or it already arrived.
    NotRequested(K),
    /// A chunk of another item arrived before the open one was finished.
    Interleaved { open: K, received: K },
    /// A chunk did not continue where the previous one ended.
    OffsetMismatch { id: K, expected: u64, received: u64 },
    /// The item is larger than the cap.
    TooLarge { id: K, limit: usize },
    /// The sender finished while a chunked item was still open.
    Unfinished(K),
    /// The sender finished without sending this many requested items.
    Missing(usize),
}

impl<K: fmt::Display> fmt::Display for TransferError<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRequested(id) => write!(f, "{id} was not requested"),
            Self::Interleaved { open, received } => write!(
                f,
                "a chunk of {received} arrived while {open} was unfinished"
            ),
            Self::OffsetMismatch {
                id,
                expected,
                received,
            } => write!(
                f,
                "a chunk of {id} starts at offset {received}, expected {expected}"
            ),
            Self::TooLarge { id, limit } => {
                write!(f, "{id} is larger than the {limit} byte limit")
            }
            Self::Unfinished(id) => write!(f, "{id} ended before its final chunk"),
            Self::Missing(count) => write!(f, "{count} requested items were not sent"),
        }
    }
}

impl<K: fmt::Debug + fmt::Display> std::error::Error for TransferError<K> {}

#[cfg(test)]
mod tests {
    use super::*;

    fn receiver(wanted: &[u32], max: usize) -> TransferReceiver<u32> {
        TransferReceiver::new(wanted.iter().copied(), max)
    }

    #[test]
    fn whole_items_are_accepted_once_each() {
        let mut r = receiver(&[1, 2], 16);
        assert_eq!(r.outstanding(), 2);
        r.accept_whole(1, 4).unwrap();
        assert_eq!(r.accept_whole(1, 4), Err(TransferError::NotRequested(1)));
        assert!(!r.is_complete());
        r.accept_whole(2, 0).unwrap();
        assert!(r.is_complete());
        r.finish().unwrap();
    }

    #[test]
    fn unrequested_items_are_refused() {
        let mut r = receiver(&[1], 16);
        assert_eq!(r.accept_whole(9, 1), Err(TransferError::NotRequested(9)));
        assert_eq!(
            r.accept_chunk(9, 0, vec![0], true),
            Err(TransferError::NotRequested(9))
        );
    }

    #[test]
    fn chunks_reassemble_in_order() {
        let mut r = receiver(&[1], 16);
        assert_eq!(r.accept_chunk(1, 0, vec![1, 2], false), Ok(None));
        assert_eq!(r.outstanding(), 1);
        assert_eq!(r.accept_chunk(1, 2, vec![3], false), Ok(None));
        assert_eq!(
            r.accept_chunk(1, 3, vec![4, 5], true),
            Ok(Some((1, vec![1, 2, 3, 4, 5])))
        );
        assert!(r.is_complete());
        // The item is done; more chunks for it are unrequested.
        assert_eq!(
            r.accept_chunk(1, 5, vec![6], true),
            Err(TransferError::NotRequested(1))
        );
    }

    #[test]
    fn a_single_final_chunk_is_a_whole_item() {
        let mut r = receiver(&[1], 16);
        assert_eq!(
            r.accept_chunk(1, 0, vec![7; 3], true),
            Ok(Some((1, vec![7; 3])))
        );
        r.finish().unwrap();
    }

    #[test]
    fn chunk_offsets_must_continue_the_item() {
        let mut r = receiver(&[1], 16);
        assert_eq!(
            r.accept_chunk(1, 4, vec![0], false),
            Err(TransferError::OffsetMismatch {
                id: 1,
                expected: 0,
                received: 4
            })
        );

        // Overlapping, gapped and repeated chunks are all refused.
        for bad in [0, 2, 4] {
            let mut r = receiver(&[1], 16);
            r.accept_chunk(1, 0, vec![0; 3], false).unwrap();
            assert_eq!(
                r.accept_chunk(1, bad, vec![0], false),
                Err(TransferError::OffsetMismatch {
                    id: 1,
                    expected: 3,
                    received: bad
                })
            );
        }
    }

    #[test]
    fn interleaved_chunks_are_refused() {
        let mut r = receiver(&[1, 2], 16);
        r.accept_chunk(1, 0, vec![0], false).unwrap();
        assert_eq!(
            r.accept_chunk(2, 0, vec![0], false),
            Err(TransferError::Interleaved {
                open: 1,
                received: 2
            })
        );
        // A whole item in between is fine, and the open item continues.
        r.accept_whole(2, 1).unwrap();
        assert_eq!(
            r.accept_chunk(1, 1, vec![1], true),
            Ok(Some((1, vec![0, 1])))
        );
        r.finish().unwrap();
    }

    #[test]
    fn items_over_the_cap_are_refused() {
        let mut r = receiver(&[1, 2], 4);
        assert_eq!(
            r.accept_whole(1, 5),
            Err(TransferError::TooLarge { id: 1, limit: 4 })
        );
        r.accept_chunk(2, 0, vec![0; 3], false).unwrap();
        assert_eq!(
            r.accept_chunk(2, 3, vec![0; 2], true),
            Err(TransferError::TooLarge { id: 2, limit: 4 })
        );

        // Exactly at the cap is fine, and the buffer does not grow past it.
        let mut r = receiver(&[1], 5);
        r.accept_chunk(1, 0, vec![0; 3], false).unwrap();
        let (_, item) = r.accept_chunk(1, 3, vec![0; 2], true).unwrap().unwrap();
        assert_eq!(item.len(), 5);
        assert!(item.capacity() <= 5, "capacity {}", item.capacity());
    }

    #[test]
    fn finishing_early_reports_what_is_missing() {
        let mut r = receiver(&[1, 2, 3], 16);
        r.accept_whole(1, 1).unwrap();
        assert_eq!(r.finish(), Err(TransferError::Missing(2)));

        let mut r = receiver(&[1], 16);
        r.accept_chunk(1, 0, vec![0], false).unwrap();
        assert_eq!(r.finish(), Err(TransferError::Unfinished(1)));
    }

    #[test]
    fn a_partial_answer_leaves_the_rest_outstanding() {
        let mut r = receiver(&[1, 2, 3], 16);
        r.accept_chunk(2, 0, vec![0], true).unwrap();
        r.accept_whole(3, 1).unwrap();
        assert_eq!(r.finish_partial(), Ok([1].into()));

        let mut r = receiver(&[1, 2], 16);
        r.accept_chunk(1, 0, vec![0], false).unwrap();
        assert_eq!(r.finish_partial(), Err(TransferError::Unfinished(1)));
    }

    #[test]
    fn an_unrequested_receiver_takes_anything_within_the_rules() {
        let mut r = TransferReceiver::unrequested(4);
        r.accept_whole(7, 4).unwrap();
        r.accept_whole(7, 4).unwrap();
        assert_eq!(
            r.accept_whole(8, 5),
            Err(TransferError::TooLarge { id: 8, limit: 4 })
        );
        r.accept_chunk(9, 0, vec![0; 2], false).unwrap();
        assert!(!r.is_complete());
        assert_eq!(
            r.accept_chunk(10, 0, vec![0], true),
            Err(TransferError::Interleaved {
                open: 9,
                received: 10
            })
        );
        assert_eq!(
            r.accept_chunk(9, 2, vec![0; 2], true),
            Ok(Some((9, vec![0; 4])))
        );
        assert!(r.is_complete());
        r.finish().unwrap();
    }
}
