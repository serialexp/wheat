//! Standalone wheat native protocol server.
//!
//! Serves one or more wheat repositories over WebSocket, implementing the
//! fetch and publish protocol from `wheat-protocol`. Each repository is
//! backed by a local `PackedObjectStore` (or a remote packed store) and a
//! `LocalPublicationStore`.

pub mod access;
pub mod config;
pub mod connection;
pub mod fetch;
pub mod publish;
pub mod registry;
pub mod server;
pub mod wire;
