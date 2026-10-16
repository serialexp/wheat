//! Helpers shared by Wheat CLI integration tests.

use std::path::Path;

use wheat_core::{
    ACTIVE_LOG_FRAME_HEADER_ENCODED_SIZE, ACTIVE_LOG_FRAME_TRAILER_ENCODED_SIZE,
    ACTIVE_LOG_HEADER_ENCODED_SIZE, decode_active_log_frame_header, decode_segment_index_page,
    encode_active_log_frame_fields, encode_active_log_frame_header,
    encode_active_log_frame_trailer,
};

/// Corrupts the stored bytes of one record in a store's active packed segment.
///
/// The segment's log stays valid (the digests of the record's frame and every later frame
/// are recomputed), so the store still opens and only reads of this record fail. `digest` is
/// the record's object or content id.
pub fn corrupt_packed_record(store: &Path, digest: [u8; 32]) {
    let path = store.join("storage/packed/active.segment");
    let mut bytes = std::fs::read(&path).unwrap();
    let mut chain = *blake3::hash(&bytes[..ACTIVE_LOG_HEADER_ENCODED_SIZE]).as_bytes();
    let mut frame = ACTIVE_LOG_HEADER_ENCODED_SIZE;
    let mut corrupted = false;
    while frame < bytes.len() {
        let header_end = frame + ACTIVE_LOG_FRAME_HEADER_ENCODED_SIZE;
        let (fields, _) = decode_active_log_frame_header(&bytes[frame..header_end]).unwrap();
        let index_start = header_end + usize::try_from(fields.payload_length).unwrap();
        let trailer_start = index_start + usize::try_from(fields.index_length).unwrap();
        let end = trailer_start + ACTIVE_LOG_FRAME_TRAILER_ENCODED_SIZE;
        let index = decode_segment_index_page(&bytes[index_start..trailer_start]).unwrap();
        if let Some(entry) = index.entries.iter().find(|entry| entry.digest == digest) {
            let last = usize::try_from(entry.offset + entry.stored_length - 1).unwrap();
            bytes[last] ^= 0x55;
            corrupted = true;
        }
        // Each frame's digests chain from the one before, so every frame is re-signed.
        let mut fields_digest = blake3::Hasher::new();
        fields_digest.update(&chain);
        fields_digest.update(&encode_active_log_frame_fields(&fields).unwrap());
        let header =
            encode_active_log_frame_header(&fields, *fields_digest.finalize().as_bytes()).unwrap();
        bytes[frame..header_end].copy_from_slice(&header);
        let mut frame_digest = blake3::Hasher::new();
        frame_digest.update(&chain);
        frame_digest.update(&bytes[frame..trailer_start]);
        chain = *frame_digest.finalize().as_bytes();
        bytes[trailer_start..end].copy_from_slice(&encode_active_log_frame_trailer(chain).unwrap());
        frame = end;
    }
    assert!(corrupted, "record is in the active segment");
    std::fs::write(&path, bytes).unwrap();
}
