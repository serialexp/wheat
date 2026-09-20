//! Remote packed object store — stores packed segments on S3-compatible object
//! storage with an in-memory index rebuilt from the catalog chain on startup.

mod catalog;
mod content;
mod segment_cache;
mod store;

pub use store::{RemotePackedStore, RemotePackedStoreConfig};
