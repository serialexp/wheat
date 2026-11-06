//! Every integration test of wheat-server, in one binary: each file under
//! `tests/` is a module here rather than a test binary of its own, since each
//! binary links most of Wheat. `crates/wheat-core/tests/test_binaries.rs` checks
//! that every file is listed.

#[path = "../support/mod.rs"]
mod support;

#[path = "../access_test.rs"]
mod access_test;
#[path = "../blocking_test.rs"]
mod blocking_test;
#[path = "../cli_test.rs"]
mod cli_test;
#[path = "../clone_fetch_publish_test.rs"]
mod clone_fetch_publish_test;
#[path = "../connection_limits_test.rs"]
mod connection_limits_test;
#[path = "../delete_test.rs"]
mod delete_test;
#[path = "../embedding_test.rs"]
mod embedding_test;
#[path = "../fetch_test.rs"]
mod fetch_test;
#[path = "../group_commit_test.rs"]
mod group_commit_test;
#[path = "../naming_test.rs"]
mod naming_test;
#[path = "../protocol_test.rs"]
mod protocol_test;
#[path = "../publish_test.rs"]
mod publish_test;
#[path = "../registry_test.rs"]
mod registry_test;
#[path = "../s3_packed_test.rs"]
mod s3_packed_test;
#[path = "../server_write_test.rs"]
mod server_write_test;
#[path = "../tls_test.rs"]
mod tls_test;
