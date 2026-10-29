//! Shared message framing — send, receive, error helpers.
//!
//! Eliminates the send/recv duplication across connection, fetch, and publish.

use wheat_core::{ContentId, ObjectId};
use wheat_protocol::*;
use wheat_store::RepositoryHead;

use crate::connection::ConnectionError;
use crate::transport::Transport;

/// Send a protocol message.
pub async fn send(
    ws: &mut impl Transport,
    message: &WheatProtocolMessage,
) -> Result<(), ConnectionError> {
    let bytes = encode_message(message).map_err(ConnectionError::Encode)?;
    ws.send(bytes.into())
        .await
        .map_err(ConnectionError::Transport)
}

/// Receive the next protocol message. Decoding refuses one larger than
/// [`MAX_MESSAGE_SIZE`].
pub async fn recv(ws: &mut impl Transport) -> Result<WheatProtocolMessage, ConnectionError> {
    let bytes = ws
        .recv()
        .await
        .map_err(ConnectionError::Transport)?
        .ok_or(ConnectionError::Closed)?;
    decode_message(&bytes).map_err(ConnectionError::Decode)
}

/// Send an Error message to the client.
pub async fn send_error(
    ws: &mut impl Transport,
    op_id: u64,
    code: u16,
    scope: ErrorScope,
    retryable: bool,
    message: &str,
) -> Result<(), ConnectionError> {
    send_error_retry_after(ws, op_id, code, scope, retryable.then_some(None), message).await
}

/// Like [`send_error`], for a retryable error when `retry` is `Some`, with the
/// server's estimate of how long until it can work, if it has one.
pub async fn send_error_retry_after(
    ws: &mut impl Transport,
    op_id: u64,
    code: u16,
    scope: ErrorScope,
    retry: Option<Option<std::time::Duration>>,
    message: &str,
) -> Result<(), ConnectionError> {
    send(
        ws,
        &WheatProtocolMessage::Error(ErrorOutput {
            message_type: 0x70,
            op_id,
            code,
            scope,
            retryable: retry.is_some(),
            retry_after_seconds: retry.flatten().map_or(0, |after| after.as_secs().max(1)),
            message: message.to_string(),
            object_id: Digest256 { bytes: vec![0; 32] },
        }),
    )
    .await
}

/// Send an operation-scoped, non-retryable Error about one object.
pub async fn send_object_error(
    ws: &mut impl Transport,
    op_id: u64,
    code: u16,
    message: &str,
    object: ObjectId,
) -> Result<(), ConnectionError> {
    send(
        ws,
        &WheatProtocolMessage::Error(ErrorOutput {
            message_type: 0x70,
            op_id,
            code,
            scope: operation_error_scope(),
            retryable: false,
            retry_after_seconds: 0,
            message: message.to_string(),
            object_id: Digest256 {
                bytes: object.as_bytes().to_vec(),
            },
        }),
    )
    .await
}

pub fn connection_error_scope() -> ErrorScope {
    ErrorScope::ErrorScopeConnection(ErrorScopeConnectionOutput { code: 0 })
}

pub fn operation_error_scope() -> ErrorScope {
    ErrorScope::ErrorScopeOperation(ErrorScopeOperationOutput { code: 1 })
}

/// Convert a wire Digest256 to an ObjectId.
pub fn digest_to_object_id(d: &Digest256) -> Result<ObjectId, ConnectionError> {
    let bytes: [u8; 32] = d
        .bytes
        .as_slice()
        .try_into()
        .map_err(|_| ConnectionError::Protocol("invalid ObjectId length"))?;
    Ok(ObjectId::from_digest(bytes))
}

/// Convert a wire Digest256 to a ContentId.
pub fn digest_to_content_id(d: &Digest256) -> Result<ContentId, ConnectionError> {
    let bytes: [u8; 32] = d
        .bytes
        .as_slice()
        .try_into()
        .map_err(|_| ConnectionError::Protocol("invalid ContentId length"))?;
    Ok(ContentId::from_digest(bytes))
}

/// Build a PublishResult message for a publish that was not published, which has no
/// receipt.
pub fn publish_result(
    op_id: u64,
    status: PublishResultStatus,
    generation: u64,
    envelope: &[u8; 32],
    message: &str,
) -> WheatProtocolMessage {
    result_with_receipt(op_id, status, [0; 16], generation, envelope, message)
}

/// Build the PublishResult of a publish published in `head`: `already_committed` when
/// it was published by an earlier sending with the same key.
pub fn publish_published(
    op_id: u64,
    repeated: bool,
    receipt: [u8; 16],
    head: RepositoryHead,
) -> WheatProtocolMessage {
    let status = if repeated {
        PublishResultStatus::PublishStatusAlreadyCommitted(PublishStatusAlreadyCommittedOutput {
            code: 3,
        })
    } else {
        publish_ok_status()
    };
    result_with_receipt(
        op_id,
        status,
        receipt,
        head.generation,
        head.envelope.as_bytes(),
        "",
    )
}

fn result_with_receipt(
    op_id: u64,
    status: PublishResultStatus,
    receipt: [u8; 16],
    generation: u64,
    envelope: &[u8; 32],
    message: &str,
) -> WheatProtocolMessage {
    WheatProtocolMessage::PublishResult(PublishResultOutput {
        message_type: 0x49,
        op_id,
        receipt_id: Uuid128 {
            bytes: receipt.to_vec(),
        },
        status,
        new_generation: generation,
        new_envelope: Digest256 {
            bytes: envelope.to_vec(),
        },
        message: message.to_string(),
    })
}

pub fn publish_ok_status() -> PublishResultStatus {
    PublishResultStatus::PublishStatusOk(PublishStatusOkOutput { code: 0 })
}

pub fn publish_stale_status() -> PublishResultStatus {
    PublishResultStatus::PublishStatusStaleHead(PublishStatusStaleHeadOutput { code: 1 })
}

pub fn publish_rejected_status() -> PublishResultStatus {
    PublishResultStatus::PublishStatusRejected(PublishStatusRejectedOutput { code: 2 })
}

pub fn publish_error_status() -> PublishResultStatus {
    PublishResultStatus::PublishStatusError(PublishStatusErrorOutput { code: 4 })
}
