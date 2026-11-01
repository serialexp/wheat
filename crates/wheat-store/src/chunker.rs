//! Content-defined chunking for blobs over [`CHUNKED_CONTENT_THRESHOLD`]: FastCDC
//! (Xia et al., 2016) with normalized chunking at level 2.
//!
//! A Gear hash rolls over the bytes; a boundary falls where its top bits are zero.
//! The hash depends only on the last 64 bytes, so a boundary depends only on the
//! bytes around it: an edit moves the boundaries near it and leaves the rest, and
//! two versions of a blob share every chunk the edit did not touch.
//!
//! Every parameter here is part of the format. Changing one changes where new
//! versions cut, and so how much they share with old ones, though never what any
//! `ContentId` names.
//!
//! [`CHUNKED_CONTENT_THRESHOLD`]: wheat_core::CHUNKED_CONTENT_THRESHOLD

use wheat_core::{MAX_CONTENT_CHUNK_BYTES, MIN_CONTENT_CHUNK_BYTES};

const MIN: usize = MIN_CONTENT_CHUNK_BYTES as usize;
const MAX: usize = MAX_CONTENT_CHUNK_BYTES as usize;
/// The nominal average, 2 MiB, where the chunker switches masks.
const NORMAL: usize = 2 << 20;
/// Before [`NORMAL`] a boundary needs 23 zero bits, two more than the average's 21,
/// so short chunks are rare; after it 19, so long chunks are too.
const MASK_BEFORE_NORMAL: u64 = !0 << (64 - 23);
const MASK_AFTER_NORMAL: u64 = !0 << (64 - 19);

/// The seed [`GEAR`] is drawn from.
const GEAR_SEED: u64 = 0x5768_6561_7443_4443; // "WheatCDC"

/// A random 64-bit value per byte value: SplitMix64 from [`GEAR_SEED`].
const GEAR: [u64; 256] = gear_table();

const fn gear_table() -> [u64; 256] {
    let mut table = [0; 256];
    let mut state = GEAR_SEED;
    let mut index = 0;
    while index < 256 {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        table[index] = z ^ (z >> 31);
        index += 1;
    }
    table
}

/// Where the chunk at the start of `data` ends, given `data` is either everything
/// left of the blob or at least [`MAX`] bytes of it.
fn cut_point(data: &[u8]) -> usize {
    if data.len() <= MIN {
        return data.len();
    }
    let end = data.len().min(MAX);
    let normal = end.min(NORMAL);
    let mut hash = 0u64;
    // Cut-point skipping: no boundary falls before the minimum, so hashing starts
    // there.
    for (offset, &byte) in data[MIN..normal].iter().enumerate() {
        hash = (hash << 1).wrapping_add(GEAR[usize::from(byte)]);
        if hash & MASK_BEFORE_NORMAL == 0 {
            return MIN + offset + 1;
        }
    }
    for (offset, &byte) in data[normal..end].iter().enumerate() {
        hash = (hash << 1).wrapping_add(GEAR[usize::from(byte)]);
        if hash & MASK_AFTER_NORMAL == 0 {
            return normal + offset + 1;
        }
    }
    end
}

/// The chunks of a blob held whole, cut where [`ContentChunker`] cuts it.
pub(crate) fn chunks_of(blob: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = blob;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (chunk, tail) = rest.split_at(cut_point(rest));
        rest = tail;
        Some(chunk)
    })
}

/// Cuts a blob fed in pieces of any size into the same chunks as fed whole. It
/// holds at most one longest chunk: bytes are copied into it only while a chunk
/// spans two pieces; a chunk inside one piece is passed on from the piece itself.
#[derive(Debug, Default)]
pub struct ContentChunker {
    pending: Vec<u8>,
}

impl ContentChunker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes held for a chunk that spans pieces.
    #[must_use]
    pub fn held_bytes(&self) -> usize {
        self.pending.capacity()
    }

    /// Feeds the next bytes of the blob, passing each chunk that ends in them to
    /// `chunk`, in order.
    pub fn update<E>(
        &mut self,
        mut input: &[u8],
        mut chunk: impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        loop {
            if self.pending.is_empty() {
                while input.len() >= MAX {
                    let cut = cut_point(input);
                    chunk(&input[..cut])?;
                    input = &input[cut..];
                }
                self.pending.extend_from_slice(input);
                return Ok(());
            }
            let held = self.pending.len();
            let take = (MAX - held).min(input.len());
            if self.pending.capacity() < MAX {
                self.pending.reserve_exact(MAX - held);
            }
            self.pending.extend_from_slice(&input[..take]);
            if self.pending.len() < MAX {
                return Ok(());
            }
            let cut = cut_point(&self.pending);
            chunk(&self.pending[..cut])?;
            if cut >= held {
                // The rest of the held bytes came from `input`: carry on from there.
                input = &input[cut - held..];
                self.pending.clear();
            } else {
                self.pending.drain(..cut);
                input = &input[take..];
            }
        }
    }

    /// Ends the blob, passing its remaining chunks to `chunk`.
    pub fn finish<E>(mut self, mut chunk: impl FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        let mut rest = self.pending.as_slice();
        while !rest.is_empty() {
            let cut = cut_point(rest);
            chunk(&rest[..cut])?;
            rest = &rest[cut..];
        }
        self.pending.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use super::*;

    const MIB: usize = 1 << 20;

    /// Deterministic incompressible bytes.
    fn noise(length: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 32) as u8
            })
            .collect()
    }

    /// The chunk lengths for `data` fed in pieces of `piece` bytes.
    fn lengths(data: &[u8], piece: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut chunker = ContentChunker::new();
        for part in data.chunks(piece) {
            chunker
                .update(part, |chunk| {
                    out.push(chunk.len());
                    Ok::<_, Infallible>(())
                })
                .unwrap();
        }
        chunker
            .finish(|chunk| {
                out.push(chunk.len());
                Ok::<_, Infallible>(())
            })
            .unwrap();
        out
    }

    fn offsets(lengths: &[usize]) -> Vec<usize> {
        lengths
            .iter()
            .scan(0, |end, length| {
                *end += length;
                Some(*end)
            })
            .collect()
    }

    #[test]
    fn the_gear_table_is_pinned() {
        let digest = blake3::hash(&GEAR.map(u64::to_le_bytes).concat());
        assert_eq!(
            (GEAR[0], GEAR[1], GEAR[255], digest.to_hex().as_str()),
            (
                14_017_348_017_320_718_528,
                734_011_478_964_513_298,
                6_972_775_926_295_239_211,
                "658c2bc2d3a19a0e02e3cae68ed7029260255ffd48e25dd6a34f982de77592a8"
            )
        );
    }

    #[test]
    fn cut_points_are_pinned() {
        assert_eq!(
            offsets(&lengths(&noise(24 * MIB, 1), usize::MAX)),
            [
                2_291_796, 3_364_962, 5_586_248, 7_956_478, 10_139_634, 12_930_690, 15_586_157,
                18_482_951, 19_977_698, 22_082_205, 24_400_557, 25_165_824
            ]
        );
    }

    #[test]
    fn chunks_are_within_the_format_sizes() {
        let data = noise(64 * MIB, 2);
        let chunks = lengths(&data, usize::MAX);
        assert_eq!(chunks.iter().sum::<usize>(), data.len());
        let (last, rest) = chunks.split_last().unwrap();
        assert!(rest.iter().all(|length| (MIN..=MAX).contains(length)));
        assert!((1..=MAX).contains(last));
        // Normalized chunking keeps the average near 2 MiB on random bytes.
        let average = data.len() / chunks.len();
        assert!((3 * MIB / 2..=3 * MIB).contains(&average), "{average}");
    }

    #[test]
    fn bytes_without_boundaries_cut_at_the_longest_chunk() {
        assert_eq!(
            lengths(&vec![0; 10 * MIB], usize::MAX),
            vec![4 * MIB, 4 * MIB, 2 * MIB]
        );
    }

    #[test]
    fn a_short_blob_is_one_chunk_and_an_empty_one_none() {
        assert_eq!(lengths(&noise(MIB, 3), usize::MAX), vec![MIB]);
        assert_eq!(
            lengths(&noise(MIB + 1, 3), usize::MAX)
                .iter()
                .sum::<usize>(),
            MIB + 1
        );
        assert!(lengths(&[], usize::MAX).is_empty());
    }

    #[test]
    fn the_cuts_do_not_depend_on_how_the_blob_is_fed() {
        let data = noise(20 * MIB, 4);
        let whole = lengths(&data, usize::MAX);
        for piece in [1, 7, 4096, MIB - 1, MAX - 1, MAX, MAX + 1, 3 * MAX + 5] {
            assert_eq!(lengths(&data, piece), whole, "fed {piece} bytes at a time");
        }
        assert_eq!(chunks_of(&data).map(<[u8]>::len).collect::<Vec<_>>(), whole);
    }

    #[test]
    fn an_edit_moves_only_the_boundaries_near_it() {
        let data = noise(48 * MIB, 5);
        let mut edited = data.clone();
        edited[24 * MIB] ^= 0xff;
        edited.splice(30 * MIB..30 * MIB, noise(1000, 6));
        let before = offsets(&lengths(&data, usize::MAX));
        let after = offsets(&lengths(&edited, usize::MAX));
        // Every boundary before the first edit stays put.
        let first: Vec<_> = before.iter().filter(|&&end| end <= 24 * MIB).collect();
        assert_eq!(
            after
                .iter()
                .filter(|&&end| end <= 24 * MIB)
                .collect::<Vec<_>>(),
            first
        );
        // Past the second edit, the boundaries resynchronize, shifted by the insertion.
        let tail = |ends: &[usize], shift: usize| {
            ends.iter()
                .filter(|&&end| end > 30 * MIB + MAX + shift)
                .map(|&end| end - shift)
                .collect::<Vec<_>>()
        };
        assert_eq!(tail(&after, 1000), tail(&before, 0));
        assert!(!tail(&before, 0).is_empty());
    }
}
