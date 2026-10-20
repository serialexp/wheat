//! Wheat native wire protocol — message types and framing.
//!
//! This crate owns the WebSocket-level protocol: message definitions
//! (generated from `wheat-protocol.bs.json5` via binschema), encoding,
//! decoding, and framing. It does not contain transport or server logic.
//!
//! # Wire format
//!
//! Each WebSocket binary message contains:
//! ```text
//! [message_type: u8] [binschema-encoded payload]
//! ```
//!
//! The `message_type` byte selects the [`WheatProtocolMessage`] variant.
//! Type bytes `0x00–0x7f` are core protocol; `0x80–0xff` are reserved for
//! extensions (unknown types in this range are ignored by older peers).
//!
//! # Size limits
//!
//! Maximum WebSocket message size is 16 MiB. Messages exceeding this are
//! rejected. The framing layer (not binschema) enforces aggregate
//! message-size bounds and per-field maximums before allocating.

mod content_batch;
mod transfer_receiver;

pub use content_batch::{ContentBatcher, content_chunks};
pub use transfer_receiver::{TransferError, TransferReceiver};
pub use wheat_protocol_generated::generated::*;
pub use wheat_protocol_generated::{BitOrder, BitStreamDecoder, BitStreamEncoder};

/// Maximum WebSocket message size (16 MiB).
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Maximum aggregate ObjectBatch payload (8 MiB).
pub const MAX_OBJECT_BATCH_SIZE: usize = 8 * 1024 * 1024;

/// Maximum aggregate ContentBatch payload (12 MiB).
pub const MAX_CONTENT_BATCH_SIZE: usize = 12 * 1024 * 1024;

/// Maximum data in one ObjectChunk (4 MiB).
pub const MAX_OBJECT_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// Maximum size of one structural object, however many chunks it arrives in
/// (1 GiB). Matches the object store's hard record limit.
pub const MAX_OBJECT_SIZE: usize = 1024 * 1024 * 1024;

/// Maximum per-item/chunk content data (4 MiB).
pub const MAX_CONTENT_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// Maximum size of one content blob, however many chunks it arrives in
/// (1 GiB). Matches the content store's hard record limit, so a peer never
/// accepts a blob it could not store.
pub const MAX_CONTENT_SIZE: usize = 1024 * 1024 * 1024;

/// Maximum items in an ObjectBatch.
pub const MAX_OBJECT_BATCH_ITEMS: usize = 256;

/// Maximum items in a ContentBatch: as many as one ContentNeeded round asks for,
/// so a round of small blobs fits one message.
pub const MAX_CONTENT_BATCH_ITEMS: usize = MAX_WANT_IDS;

/// Maximum IDs in an ObjectWant or ContentFetchRequest.
pub const MAX_WANT_IDS: usize = 4096;

/// Largest blob a publishing client pushes before the server asks (256 KiB). A
/// larger one waits for a ContentNeeded round, so a blob the server already holds
/// costs that round rather than its bytes.
pub const MAX_PUSHED_BLOB_SIZE: usize = 256 * 1024;

/// Most object and blob bytes a client pushes, all told, before `PublishPushEnd`
/// (32 MiB). The server holds them until its walk claims them; what does not fit
/// is asked for.
pub const MAX_PUSHED_BYTES: usize = 32 * 1024 * 1024;

/// Decode a protocol message from raw WebSocket binary message bytes.
///
/// Returns `Err` if the message is empty, exceeds 16 MiB, or the
/// binschema decoder rejects the payload.
pub fn decode_message(bytes: &[u8]) -> Result<WheatProtocolMessage, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    if bytes.len() > MAX_MESSAGE_SIZE {
        return Err(DecodeError::TooLarge(bytes.len()));
    }
    WheatProtocolMessage::decode(bytes).map_err(|e| DecodeError::Schema(e.to_string()))
}

/// Encode a protocol message into raw bytes suitable for a WebSocket
/// binary message.
pub fn encode_message(message: &WheatProtocolMessage) -> Result<Vec<u8>, EncodeError> {
    let bytes = message
        .encode()
        .map_err(|e| EncodeError::Schema(e.to_string()))?;
    if bytes.len() > MAX_MESSAGE_SIZE {
        return Err(EncodeError::TooLarge(bytes.len()));
    }
    Ok(bytes)
}

/// Error decoding a protocol message.
#[derive(Debug)]
pub enum DecodeError {
    /// Empty message (no type byte).
    Empty,
    /// Message exceeds the 16 MiB limit.
    TooLarge(usize),
    /// Binschema decode error.
    Schema(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Empty => write!(f, "empty protocol message"),
            DecodeError::TooLarge(size) => {
                write!(
                    f,
                    "protocol message too large: {size} bytes (max {MAX_MESSAGE_SIZE})"
                )
            }
            DecodeError::Schema(e) => write!(f, "protocol decode error: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Error encoding a protocol message.
#[derive(Debug)]
pub enum EncodeError {
    /// Encoded message exceeds the 16 MiB limit.
    TooLarge(usize),
    /// Binschema encode error.
    Schema(String),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::TooLarge(size) => {
                write!(
                    f,
                    "encoded protocol message too large: {size} bytes (max {MAX_MESSAGE_SIZE})"
                )
            }
            EncodeError::Schema(e) => write!(f, "protocol encode error: {e}"),
        }
    }
}

impl std::error::Error for EncodeError {}
