//! Protocol messages come from the network, so their length fields are
//! untrusted. A forged length must be a decode error, not a reservation of
//! everything it claims.

use wheat_protocol::{
    DecodeError, Digest256, ObjectBatchOutput, ObjectEntry, WheatProtocolMessage, decode_message,
    encode_message,
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
