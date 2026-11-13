//! Standalone wheat native protocol server.
//!
//! Serves one or more wheat repositories over WebSocket, implementing the
//! fetch and publish protocol from `wheat-protocol`. Each repository is
//! backed by a local `PackedObjectStore` (or a remote packed store) and a
//! [`heads::HeadStore`], by default a `LocalPublicationStore`.
//!
//! The default `standalone` feature builds the `wheat-server` binary, with its
//! command line, allocator, log output, TLS (`tls`) and config file (`config-file`).
//! An embedder that serves repositories inside its own process sets
//! `default-features = false` and turns on only what it uses.

/// The attribute that implements [`access::AccessProvider`] and
/// [`hooks::PublishHooks`], so an embedder needs no version of its own.
pub use async_trait::async_trait;

pub mod access;
pub mod admin;
mod blocking;
pub mod commit;
pub mod config;
pub mod connection;
mod error_text;
pub mod fetch;
pub mod heads;
pub mod hooks;
pub mod naming;
pub mod network;
pub mod publish;
pub mod quiesce;
pub mod receipt;
pub mod registry;
pub mod server;
#[cfg(feature = "tls")]
pub mod tls;
pub mod transport;
pub mod wire;
pub mod write;
