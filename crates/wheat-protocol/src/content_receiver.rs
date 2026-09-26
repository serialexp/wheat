//! Receiving requested content, whole or in chunks.
//!
//! Both ends of the protocol receive content the other side sent because it
//! was asked for: the client after a `ContentFetchRequest`, the server after
//! `ContentNeeded`. The sender is untrusted, so [`ContentReceiver`] refuses
//! anything that was not asked for, anything asked for twice, chunks that do
//! not continue where the last one stopped, and blobs larger than the cap.
//! It only tracks identities; hashing the finished bytes is the caller's job.

use std::collections::HashSet;
use std::fmt;
use std::hash::Hash;

/// Collects the content asked for in one request.
///
/// A blob arrives either whole or as chunks with `offset` equal to the bytes
/// received so far. Chunks of one blob must not be interleaved with chunks of
/// another, though whole blobs may arrive in between.
#[derive(Debug)]
pub struct ContentReceiver<K> {
    wanted: HashSet<K>,
    partial: Option<(K, Vec<u8>)>,
    max_content_bytes: usize,
}

impl<K: Copy + Eq + Hash + fmt::Debug> ContentReceiver<K> {
    /// Expect each of `wanted` once, none larger than `max_content_bytes`.
    pub fn new(wanted: impl IntoIterator<Item = K>, max_content_bytes: usize) -> Self {
        Self {
            wanted: wanted.into_iter().collect(),
            partial: None,
            max_content_bytes,
        }
    }

    /// Accept a blob that arrived in one message.
    pub fn accept_whole(&mut self, id: K, len: usize) -> Result<(), ContentReceiveError<K>> {
        if len > self.max_content_bytes {
            return Err(ContentReceiveError::TooLarge {
                id,
                limit: self.max_content_bytes,
            });
        }
        if !self.wanted.remove(&id) {
            return Err(ContentReceiveError::NotRequested(id));
        }
        Ok(())
    }

    /// Accept one chunk. Returns the finished blob after its final chunk.
    pub fn accept_chunk(
        &mut self,
        id: K,
        offset: u64,
        data: Vec<u8>,
        is_final: bool,
    ) -> Result<Option<(K, Vec<u8>)>, ContentReceiveError<K>> {
        let mut buf = match self.partial.take() {
            Some((open, buf)) if open == id => buf,
            Some((open, buf)) => {
                self.partial = Some((open, buf));
                return Err(ContentReceiveError::Interleaved { open, received: id });
            }
            None => {
                if !self.wanted.remove(&id) {
                    return Err(ContentReceiveError::NotRequested(id));
                }
                Vec::new()
            }
        };

        if offset != buf.len() as u64 {
            return Err(ContentReceiveError::OffsetMismatch {
                id,
                expected: buf.len() as u64,
                received: offset,
            });
        }
        let new_len = match buf.len().checked_add(data.len()) {
            Some(len) if len <= self.max_content_bytes => len,
            _ => {
                return Err(ContentReceiveError::TooLarge {
                    id,
                    limit: self.max_content_bytes,
                });
            }
        };

        if buf.is_empty() {
            // The first chunk's buffer becomes the blob; a single-chunk blob
            // is never copied.
            buf = data;
        } else {
            if new_len > buf.capacity() {
                // Grow by doubling, but never past the cap, so a blob at the
                // cap does not hold twice its size.
                let target = buf
                    .capacity()
                    .saturating_mul(2)
                    .max(new_len)
                    .min(self.max_content_bytes);
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

    /// True once every requested blob has arrived in full.
    pub fn is_complete(&self) -> bool {
        self.wanted.is_empty() && self.partial.is_none()
    }

    /// Blobs not yet received in full, counting one still arriving in chunks.
    pub fn outstanding(&self) -> usize {
        self.wanted.len() + usize::from(self.partial.is_some())
    }

    /// Check that nothing is left, when the sender says it is done.
    pub fn finish(self) -> Result<(), ContentReceiveError<K>> {
        if let Some((id, _)) = self.partial {
            return Err(ContentReceiveError::Unfinished(id));
        }
        if !self.wanted.is_empty() {
            return Err(ContentReceiveError::Missing(self.wanted.len()));
        }
        Ok(())
    }
}

/// Why received content was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum ContentReceiveError<K> {
    /// The blob was not asked for, or it already arrived.
    NotRequested(K),
    /// A chunk of another blob arrived before the open one was finished.
    Interleaved { open: K, received: K },
    /// A chunk did not continue where the previous one ended.
    OffsetMismatch { id: K, expected: u64, received: u64 },
    /// The blob is larger than the cap.
    TooLarge { id: K, limit: usize },
    /// The sender finished while a chunked blob was still open.
    Unfinished(K),
    /// The sender finished without sending this many requested blobs.
    Missing(usize),
}

impl<K: fmt::Display> fmt::Display for ContentReceiveError<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRequested(id) => write!(f, "content {id} was not requested"),
            Self::Interleaved { open, received } => write!(
                f,
                "chunk for content {received} arrived while content {open} was unfinished"
            ),
            Self::OffsetMismatch {
                id,
                expected,
                received,
            } => write!(
                f,
                "chunk for content {id} starts at offset {received}, expected {expected}"
            ),
            Self::TooLarge { id, limit } => {
                write!(f, "content {id} is larger than the {limit} byte limit")
            }
            Self::Unfinished(id) => write!(f, "content {id} ended before its final chunk"),
            Self::Missing(count) => write!(f, "{count} requested content blobs were not sent"),
        }
    }
}

impl<K: fmt::Debug + fmt::Display> std::error::Error for ContentReceiveError<K> {}

#[cfg(test)]
mod tests {
    use super::*;

    fn receiver(wanted: &[u32], max: usize) -> ContentReceiver<u32> {
        ContentReceiver::new(wanted.iter().copied(), max)
    }

    #[test]
    fn whole_blobs_are_accepted_once_each() {
        let mut r = receiver(&[1, 2], 16);
        assert_eq!(r.outstanding(), 2);
        r.accept_whole(1, 4).unwrap();
        assert_eq!(
            r.accept_whole(1, 4),
            Err(ContentReceiveError::NotRequested(1))
        );
        assert!(!r.is_complete());
        r.accept_whole(2, 0).unwrap();
        assert!(r.is_complete());
        r.finish().unwrap();
    }

    #[test]
    fn unrequested_content_is_refused() {
        let mut r = receiver(&[1], 16);
        assert_eq!(
            r.accept_whole(9, 1),
            Err(ContentReceiveError::NotRequested(9))
        );
        assert_eq!(
            r.accept_chunk(9, 0, vec![0], true),
            Err(ContentReceiveError::NotRequested(9))
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
        // The blob is done; more chunks for it are unrequested.
        assert_eq!(
            r.accept_chunk(1, 5, vec![6], true),
            Err(ContentReceiveError::NotRequested(1))
        );
    }

    #[test]
    fn a_single_final_chunk_is_a_whole_blob() {
        let mut r = receiver(&[1], 16);
        assert_eq!(
            r.accept_chunk(1, 0, vec![7; 3], true),
            Ok(Some((1, vec![7; 3])))
        );
        r.finish().unwrap();
    }

    #[test]
    fn chunk_offsets_must_continue_the_blob() {
        let mut r = receiver(&[1], 16);
        assert_eq!(
            r.accept_chunk(1, 4, vec![0], false),
            Err(ContentReceiveError::OffsetMismatch {
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
                Err(ContentReceiveError::OffsetMismatch {
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
            Err(ContentReceiveError::Interleaved {
                open: 1,
                received: 2
            })
        );
        // A whole blob in between is fine, and the open blob continues.
        r.accept_whole(2, 1).unwrap();
        assert_eq!(
            r.accept_chunk(1, 1, vec![1], true),
            Ok(Some((1, vec![0, 1])))
        );
        r.finish().unwrap();
    }

    #[test]
    fn blobs_over_the_cap_are_refused() {
        let mut r = receiver(&[1, 2], 4);
        assert_eq!(
            r.accept_whole(1, 5),
            Err(ContentReceiveError::TooLarge { id: 1, limit: 4 })
        );
        r.accept_chunk(2, 0, vec![0; 3], false).unwrap();
        assert_eq!(
            r.accept_chunk(2, 3, vec![0; 2], true),
            Err(ContentReceiveError::TooLarge { id: 2, limit: 4 })
        );

        // Exactly at the cap is fine, and the buffer does not grow past it.
        let mut r = receiver(&[1], 5);
        r.accept_chunk(1, 0, vec![0; 3], false).unwrap();
        let (_, blob) = r.accept_chunk(1, 3, vec![0; 2], true).unwrap().unwrap();
        assert_eq!(blob.len(), 5);
        assert!(blob.capacity() <= 5, "capacity {}", blob.capacity());
    }

    #[test]
    fn finishing_early_reports_what_is_missing() {
        let mut r = receiver(&[1, 2, 3], 16);
        r.accept_whole(1, 1).unwrap();
        assert_eq!(r.finish(), Err(ContentReceiveError::Missing(2)));

        let mut r = receiver(&[1], 16);
        r.accept_chunk(1, 0, vec![0], false).unwrap();
        assert_eq!(r.finish(), Err(ContentReceiveError::Unfinished(1)));
    }
}
