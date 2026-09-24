//! Helpers shared by Wheat CLI integration tests.

use std::path::Path;

use wheat_core::{
    SEGMENT_FOOTER_ENCODED_SIZE, decode_segment_directory, decode_segment_footer,
    decode_segment_index_page, encode_segment_footer,
};

/// Corrupts the stored bytes of one record in a store's active packed segment.
///
/// The segment stays structurally valid (its body digest is recomputed), so the store still
/// opens and only reads of this record fail. `digest` is the record's object or content id.
pub fn corrupt_packed_record(store: &Path, digest: [u8; 32]) {
    let path = store.join("storage/packed/active.segment");
    let mut bytes = std::fs::read(&path).unwrap();
    let footer_at = bytes.len() - SEGMENT_FOOTER_ENCODED_SIZE;
    let mut footer = decode_segment_footer(&bytes[footer_at..]).unwrap();
    let directory_start = usize::try_from(footer.directory_offset).unwrap();
    let directory_end = directory_start + usize::try_from(footer.directory_length).unwrap();
    let directory = decode_segment_directory(&bytes[directory_start..directory_end]).unwrap();
    let entry = directory
        .pages
        .iter()
        .flat_map(|page| {
            let start = usize::try_from(page.offset).unwrap();
            let end = start + usize::try_from(page.length).unwrap();
            decode_segment_index_page(&bytes[start..end])
                .unwrap()
                .entries
        })
        .find(|entry| entry.digest == digest)
        .expect("record is in the active segment");
    let last = usize::try_from(entry.offset + entry.stored_length - 1).unwrap();
    bytes[last] ^= 0x55;
    footer.body_digest = *blake3::hash(&bytes[..footer_at]).as_bytes();
    bytes[footer_at..].copy_from_slice(&encode_segment_footer(&footer).unwrap());
    std::fs::write(&path, bytes).unwrap();
}
