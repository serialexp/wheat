//! Smoke tests: encode a protocol message, decode, compare.

use wheat_protocol::{
    CreditOutput, Digest256, ErrorOutput, ErrorScope, ErrorScopeConnectionOutput,
    FetchCompleteOutput, FetchRequestOutput, ObjectBatchOutput, ObjectEntry, PublishRequestOutput,
    PublishResultOutput, PublishResultStatus, PublishStatusOkOutput, RepositoryOpenOutput,
    ServerHelloOutput, SessionEstablishedOutput, SessionEstablishedStatus, SessionStatusOkOutput,
    Uuid128, WheatProtocolMessage, decode_message, encode_message,
};

fn zero_digest() -> Digest256 {
    Digest256 { bytes: vec![0; 32] }
}

fn zero_uuid() -> Uuid128 {
    Uuid128 { bytes: vec![0; 16] }
}

fn round_trip(message: WheatProtocolMessage) -> WheatProtocolMessage {
    let bytes = encode_message(&message).expect("encode failed");
    decode_message(&bytes).expect("decode failed")
}

#[test]
fn server_hello_round_trip() {
    let msg = WheatProtocolMessage::ServerHello(ServerHelloOutput {
        message_type: 0x01,
        protocol_version_min: 1,
        protocol_version_max: 1,
        server_id: vec![0xab; 32],
        capabilities: vec!["wheat-objects-v0.1".to_string()],
    });
    let decoded = round_trip(msg.clone());
    assert_eq!(msg, decoded);
}

#[test]
fn fetch_request_round_trip() {
    let msg = WheatProtocolMessage::FetchRequest(FetchRequestOutput {
        message_type: 0x20,
        op_id: 42,
        labels: vec!["main".to_string(), "develop".to_string()],
        since_generation: 0,
        since_envelope: zero_digest(),
    });
    let decoded = round_trip(msg.clone());
    assert_eq!(msg, decoded);
}

#[test]
fn object_batch_round_trip() {
    let msg = WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
        message_type: 0x21,
        op_id: 1,
        sequence: 0,
        objects: vec![ObjectEntry {
            id: Digest256 {
                bytes: vec![0xff; 32],
            },
            data: vec![1, 2, 3, 4],
        }],
    });
    let decoded = round_trip(msg.clone());
    assert_eq!(msg, decoded);
}

#[test]
fn publish_request_round_trip() {
    let msg = WheatProtocolMessage::PublishRequest(PublishRequestOutput {
        message_type: 0x40,
        op_id: 7,
        publish_key: zero_uuid(),
        envelope_id: zero_digest(),
        expected_generation: 5,
        expected_envelope: zero_digest(),
        force: false,
    });
    let decoded = round_trip(msg.clone());
    assert_eq!(msg, decoded);
}

#[test]
fn publish_result_round_trip() {
    let msg = WheatProtocolMessage::PublishResult(PublishResultOutput {
        message_type: 0x49,
        op_id: 7,
        receipt_id: zero_uuid(),
        status: PublishResultStatus::PublishStatusOk(PublishStatusOkOutput { code: 0 }),
        new_generation: 6,
        new_envelope: zero_digest(),
        message: String::new(),
    });
    let decoded = round_trip(msg.clone());
    assert_eq!(msg, decoded);
}

#[test]
fn credit_round_trip() {
    let msg = WheatProtocolMessage::Credit(CreditOutput {
        message_type: 0x60,
        op_id: 0,
        ack_through_sequence: 10,
        window_bytes: 1_048_576,
        window_messages: 64,
    });
    let decoded = round_trip(msg.clone());
    assert_eq!(msg, decoded);
}

#[test]
fn error_round_trip() {
    let msg = WheatProtocolMessage::Error(ErrorOutput {
        message_type: 0x70,
        op_id: 3,
        code: 404,
        scope: ErrorScope::ErrorScopeConnection(ErrorScopeConnectionOutput { code: 0 }),
        retryable: false,
        message: "repository not found".to_string(),
        object_id: zero_digest(),
    });
    let decoded = round_trip(msg.clone());
    assert_eq!(msg, decoded);
}

#[test]
fn session_established_round_trip() {
    let msg = WheatProtocolMessage::SessionEstablished(SessionEstablishedOutput {
        message_type: 0x03,
        selected_version: 1,
        receive_window_bytes: 4_194_304,
        receive_window_messages: 128,
        status: SessionEstablishedStatus::SessionStatusOk(SessionStatusOkOutput { code: 0 }),
        supported_range_min: 1,
        supported_range_max: 1,
        message: String::new(),
    });
    let decoded = round_trip(msg.clone());
    assert_eq!(msg, decoded);
}

#[test]
fn decode_empty_is_error() {
    assert!(decode_message(&[]).is_err());
}

#[test]
fn repository_open_round_trip() {
    let msg = WheatProtocolMessage::RepositoryOpen(RepositoryOpenOutput {
        message_type: 0x10,
        name: "aeolun/wheat-test".to_string(),
    });
    let decoded = round_trip(msg.clone());
    assert_eq!(msg, decoded);
}

#[test]
fn fetch_complete_round_trip() {
    let msg = WheatProtocolMessage::FetchComplete(FetchCompleteOutput {
        message_type: 0x23,
        op_id: 99,
    });
    let decoded = round_trip(msg.clone());
    assert_eq!(msg, decoded);
}
