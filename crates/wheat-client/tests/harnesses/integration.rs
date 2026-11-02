//! Every integration test of wheat-client, in one binary: each file under
//! `tests/` is a module here rather than a test binary of its own, since each
//! binary links most of Wheat. `crates/wheat-core/tests/test_binaries.rs` checks
//! that every file is listed.

#[path = "../served_diff.rs"]
mod served_diff;
