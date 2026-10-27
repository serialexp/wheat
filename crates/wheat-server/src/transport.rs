//! What a session runs on: a connection that carries binary messages.
//!
//! [`connection::serve`](crate::connection::serve) drives a session over any
//! [`Transport`], so an embedder that upgrades the HTTP request itself (axum's
//! `WebSocketUpgrade`, for example) serves Wheat on its own WebSocket type by
//! implementing these three methods. Wheat implements it for tungstenite's
//! [`WebSocketStream`] over any socket, which [`run`](crate::server::run) and
//! [`connection::serve_upgraded`](crate::connection::serve_upgraded) use.
//!
//! A transport sits on the trust boundary. It must refuse a message larger than
//! [`wheat_protocol::MAX_MESSAGE_SIZE`] while it arrives, not after it is in memory
//! (with axum, `WebSocketUpgrade::max_message_size` and `max_frame_size`). The
//! session refuses to decode one too, but by then the memory is spent.

use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tracing::debug;

/// A connection carrying the protocol's binary messages.
pub trait Transport: Send {
    /// Sends one binary message.
    fn send(&mut self, message: Bytes) -> impl Future<Output = Result<(), TransportError>> + Send;

    /// The next binary message, or `None` once the peer has closed the connection.
    /// Control messages (ping, pong) are the transport's own business; a text
    /// message is an error.
    fn recv(&mut self) -> impl Future<Output = Result<Option<Bytes>, TransportError>> + Send;

    /// Ends the connection once the session is over, whether it ended cleanly or not.
    /// The client may still be sending (a request sent without waiting for the
    /// session, or the rest of a push); closing with those bytes unread resets the
    /// connection, and the client can lose the reply that says why it ended. So a
    /// transport that can should start the closing handshake and read and drop what
    /// arrives until the peer answers it, for a bounded time.
    fn close(&mut self) -> impl Future<Output = ()> + Send;
}

/// A failure of the connection under a session.
#[derive(Debug)]
pub struct TransportError(Box<dyn std::error::Error + Send + Sync>);

impl TransportError {
    pub fn new(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(error.into())
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// How long [`WebSocketStream`]'s [`Transport::close`] waits for the client to answer
/// the closing handshake.
const CLOSE_WAIT: Duration = Duration::from_secs(10);

impl<S> Transport for WebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    async fn send(&mut self, message: Bytes) -> Result<(), TransportError> {
        SinkExt::send(self, Message::Binary(message))
            .await
            .map_err(TransportError::new)
    }

    async fn recv(&mut self) -> Result<Option<Bytes>, TransportError> {
        loop {
            let Some(message) = self.next().await else {
                return Ok(None);
            };
            match message.map_err(TransportError::new)? {
                Message::Binary(bytes) => return Ok(Some(bytes)),
                Message::Ping(data) => SinkExt::send(self, Message::Pong(data))
                    .await
                    .map_err(TransportError::new)?,
                Message::Pong(_) | Message::Frame(_) => {}
                Message::Close(_) => return Ok(None),
                Message::Text(_) => {
                    return Err(TransportError::new("text messages are not supported"));
                }
            }
        }
    }

    async fn close(&mut self) {
        // The client closed first: tungstenite has answered it already.
        if SinkExt::close(self).await.is_err() {
            return;
        }
        let drain = async { while let Some(Ok(_)) = self.next().await {} };
        if tokio::time::timeout(CLOSE_WAIT, drain).await.is_err() {
            debug!("client did not finish the closing handshake");
        }
    }
}
