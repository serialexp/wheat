//! Protocol messages come from the network, so their length fields are
//! untrusted. A forged length must be a decode error, not a reservation of
//! everything it claims.

use wheat_protocol::{
    DecodeError, Digest256, ObjectBatchOutput, ObjectChunkOutput, ObjectEntry,
    ObjectWantReplyOutput, WheatProtocolMessage, decode_message, encode_message,
};

#[test]
fn a_forged_object_data_length_is_a_decode_error() {
    let batch = WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
        message_type: 0x21,
        op_id: 1,
        sequence: 0,
        objects: vec![ObjectEntry {
            id: Digest256 { bytes: vec![0; 32] },
            data: Vec::new(),
        }],
    });
    let mut bytes = encode_message(&batch).unwrap();
    // The message ends with the one entry's u32 data length, which is 0.
    let end = bytes.len();
    assert_eq!(bytes[end - 4..], [0, 0, 0, 0]);
    bytes[end - 4..].copy_from_slice(&u32::MAX.to_be_bytes());

    let result = decode_message(&bytes);
    assert!(matches!(result, Err(DecodeError::Schema(_))), "{result:?}");
}

#[test]
fn a_forged_want_reply_or_chunk_length_is_a_decode_error() {
    let reply = WheatProtocolMessage::ObjectWantReply(ObjectWantReplyOutput {
        message_type: 0x25,
        op_id: 1,
        want_sequence: 0,
        objects: vec![ObjectEntry {
            id: Digest256 { bytes: vec![0; 32] },
            data: Vec::new(),
        }],
    });
    let mut bytes = encode_message(&reply).unwrap();
    // Ends with the one entry's u32 data length, which is 0.
    let end = bytes.len();
    assert_eq!(bytes[end - 4..], [0, 0, 0, 0]);
    bytes[end - 4..].copy_from_slice(&u32::MAX.to_be_bytes());
    let result = decode_message(&bytes);
    assert!(matches!(result, Err(DecodeError::Schema(_))), "{result:?}");

    let chunk = WheatProtocolMessage::ObjectChunk(ObjectChunkOutput {
        message_type: 0x26,
        op_id: 1,
        object_id: Digest256 { bytes: vec![0; 32] },
        offset: 0,
        data: Vec::new(),
        is_final: true,
    });
    let mut bytes = encode_message(&chunk).unwrap();
    // Ends with the u32 data length (0), then the one-byte is_final.
    let end = bytes.len();
    assert_eq!(bytes[end - 5..end - 1], [0, 0, 0, 0]);
    bytes[end - 5..end - 1].copy_from_slice(&u32::MAX.to_be_bytes());
    let result = decode_message(&bytes);
    assert!(matches!(result, Err(DecodeError::Schema(_))), "{result:?}");
}
