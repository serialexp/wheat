//! Smoke tests: encode a protocol message, decode, compare.

use wheat_protocol::{
    ClientHelloOutput, CreditOutput, Digest256, ErrorOutput, ErrorScope,
    ErrorScopeConnectionOutput, ErrorScopeOperationOutput, FetchCompleteOutput, FetchPushEndOutput,
    FetchRequestOutput, ObjectBatchOutput, ObjectChunkOutput, ObjectEntry, ObjectWantOutput,
    ObjectWantReplyOutput, PublishPushEndOutput, PublishRequestOutput, PublishResultOutput,
    PublishResultStatus, PublishStatusOkOutput, ServerHelloOutput, SessionEstablishedOutput,
    SessionEstablishedStatus, SessionStatusOkOutput, Uuid128, WheatProtocolMessage, decode_message,
    encode_message,
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
fn client_hello_round_trip() {
    let msg = WheatProtocolMessage::ClientHello(ClientHelloOutput {
        message_type: 0x02,
        selected_version: 1,
        receive_window_bytes: 16 * 1024 * 1024,
        receive_window_messages: 256,
        capabilities: vec!["resume".to_string(), "lazy-content".to_string()],
    });
    let decoded = round_trip(msg.clone());
    assert_eq!(msg, decoded);
}

#[test]
fn fetch_request_round_trip() {
    let msg = WheatProtocolMessage::FetchRequest(FetchRequestOutput {
        message_type: 0x20,
        op_id: 42,
        want_envelope: Digest256 {
            bytes: vec![0x5a; 32],
        },
        since_envelope: zero_digest(),
    });
    let decoded = round_trip(msg.clone());
    assert_eq!(msg, decoded);
}

#[test]
fn fetch_pull_messages_round_trip() {
    let entry = ObjectEntry {
        id: Digest256 {
            bytes: vec![0x11; 32],
        },
        data: vec![9, 8, 7],
    };
    for msg in [
        WheatProtocolMessage::FetchPushEnd(FetchPushEndOutput {
            message_type: 0x24,
            op_id: 1,
            pushed_objects: 1 << 40,
            incremental: true,
        }),
        WheatProtocolMessage::ObjectWant(ObjectWantOutput {
            message_type: 0x22,
            op_id: 1,
            want_sequence: 3,
            ids: vec![entry.id.clone(), zero_digest()],
        }),
        WheatProtocolMessage::ObjectWantReply(ObjectWantReplyOutput {
            message_type: 0x25,
            op_id: 1,
            want_sequence: 3,
            objects: vec![entry.clone()],
        }),
        WheatProtocolMessage::ObjectChunk(ObjectChunkOutput {
            message_type: 0x26,
            op_id: 1,
            object_id: entry.id.clone(),
            offset: 4 << 20,
            data: vec![1; 16],
            is_final: false,
        }),
        WheatProtocolMessage::FetchComplete(FetchCompleteOutput {
            message_type: 0x23,
            op_id: 1,
        }),
    ] {
        assert_eq!(round_trip(msg.clone()), msg);
    }
}

#[test]
fn publish_push_end_round_trips() {
    let msg = WheatProtocolMessage::PublishPushEnd(PublishPushEndOutput {
        message_type: 0x4b,
        op_id: 7,
        pushed_objects: 300,
        pushed_contents: u64::MAX,
    });
    assert_eq!(round_trip(msg.clone()), msg);
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
        force_labels: true,
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
        retry_after_seconds: 0,
        message: "repository not found".to_string(),
        object_id: zero_digest(),
    });
    let decoded = round_trip(msg.clone());
    assert_eq!(msg, decoded);

    let msg = WheatProtocolMessage::Error(ErrorOutput {
        message_type: 0x70,
        op_id: 4,
        code: 503,
        scope: ErrorScope::ErrorScopeOperation(ErrorScopeOperationOutput { code: 1 }),
        retryable: true,
        retry_after_seconds: 300,
        message: "moving".to_string(),
        object_id: zero_digest(),
    });
    assert_eq!(msg, round_trip(msg.clone()));
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
fn fetch_complete_round_trip() {
    let msg = WheatProtocolMessage::FetchComplete(FetchCompleteOutput {
        message_type: 0x23,
        op_id: 99,
    });
    let decoded = round_trip(msg.clone());
    assert_eq!(msg, decoded);
}

#[test]
fn admin_messages_round_trip() {
    use wheat_protocol::{
        AdminAdoptCommandOutput, AdminCommand, AdminExportCommandOutput, AdminFailedOutput,
        AdminImportCommandOutput, AdminRepositoryName, AdminStartOutput, AdminStartedOutput,
        AdminState, AdminStatusOutput, AdminStatusRequestOutput, AdminSucceededOutput,
    };

    let commands = [
        AdminCommand::AdminExportCommand(AdminExportCommandOutput {
            command_kind: 1,
            repository: "team/repo".to_string(),
            bundle: "bundle".to_string(),
        }),
        AdminCommand::AdminImportCommand(AdminImportCommandOutput {
            command_kind: 2,
            bundle: "bundle".to_string(),
            names: vec![AdminRepositoryName {
                repository: zero_uuid(),
                name: "team/repo".to_string(),
            }],
        }),
        AdminCommand::AdminAdoptCommand(AdminAdoptCommandOutput {
            command_kind: 3,
            source: "standalone".to_string(),
            name: "team/repo".to_string(),
        }),
    ];
    for command in commands {
        let msg = WheatProtocolMessage::AdminStart(AdminStartOutput {
            message_type: 0x50,
            op_id: 7,
            command,
        });
        assert_eq!(msg, round_trip(msg.clone()));
    }

    let started = WheatProtocolMessage::AdminStarted(AdminStartedOutput {
        message_type: 0x51,
        op_id: 7,
        operation: 3,
    });
    assert_eq!(started, round_trip(started.clone()));
    let request = WheatProtocolMessage::AdminStatusRequest(AdminStatusRequestOutput {
        message_type: 0x52,
        op_id: 8,
        operation: 3,
    });
    assert_eq!(request, round_trip(request.clone()));
    for state in [
        AdminState::AdminSucceeded(AdminSucceededOutput { code: 1 }),
        AdminState::AdminFailed(AdminFailedOutput { code: 2 }),
    ] {
        let status = WheatProtocolMessage::AdminStatus(AdminStatusOutput {
            message_type: 0x53,
            op_id: 8,
            operation: 3,
            state,
            phase: "copying".to_string(),
            message: "ok".to_string(),
        });
        assert_eq!(status, round_trip(status.clone()));
    }
}
