//! Shared WebSocket message framing — send, receive, error helpers.
//!
//! Eliminates the send/recv duplication across connection, fetch, and publish.

use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use wheat_core::{ContentId, ObjectId};
use wheat_protocol::*;

use crate::connection::ConnectionError;

/// Send a protocol message as a WebSocket binary message.
pub async fn send(
    ws: &mut WebSocketStream<TcpStream>,
    message: &WheatProtocolMessage,
) -> Result<(), ConnectionError> {
    let bytes = encode_message(message).map_err(ConnectionError::Encode)?;
    ws.send(Message::Binary(bytes.into())).await?;
    Ok(())
}

/// Receive the next binary protocol message, handling ping/pong.
pub async fn recv(
    ws: &mut WebSocketStream<TcpStream>,
) -> Result<WheatProtocolMessage, ConnectionError> {
    loop {
        let msg = ws
            .next()
            .await
            .ok_or(ConnectionError::Closed)?
            .map_err(ConnectionError::WebSocket)?;
        match msg {
            Message::Binary(bytes) => {
                return decode_message(&bytes).map_err(ConnectionError::Decode);
            }
            Message::Ping(data) => {
                ws.send(Message::Pong(data)).await?;
            }
            Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => return Err(ConnectionError::Closed),
            Message::Text(_) => {
                return Err(ConnectionError::Protocol("text frames not supported"));
            }
        }
    }
}

/// Send an Error message to the client.
pub async fn send_error(
    ws: &mut WebSocketStream<TcpStream>,
    op_id: u32,
    code: u16,
    scope: ErrorScope,
    retryable: bool,
    message: &str,
) -> Result<(), ConnectionError> {
    send(
        ws,
        &WheatProtocolMessage::Error(ErrorOutput {
            message_type: 0x70,
            op_id,
            code,
            scope,
            retryable,
            message: message.to_string(),
            object_id: Digest256 { bytes: vec![0; 32] },
        }),
    )
    .await
}

/// Send an operation-scoped, non-retryable Error about one object.
pub async fn send_object_error(
    ws: &mut WebSocketStream<TcpStream>,
    op_id: u32,
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

/// Build a PublishResult message.
pub fn publish_result(
    op_id: u32,
    status: PublishResultStatus,
    generation: u64,
    envelope: &[u8; 32],
    message: &str,
) -> WheatProtocolMessage {
    WheatProtocolMessage::PublishResult(PublishResultOutput {
        message_type: 0x49,
        op_id,
        receipt_id: Uuid128 { bytes: vec![0; 16] },
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
