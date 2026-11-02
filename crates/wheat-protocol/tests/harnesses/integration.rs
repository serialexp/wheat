//! Every integration test of wheat-protocol, in one binary: each file under
//! `tests/` is a module here rather than a test binary of its own, since each
//! binary links most of Wheat. `crates/wheat-core/tests/test_binaries.rs` checks
//! that every file is listed.

#[path = "../forged_lengths.rs"]
mod forged_lengths;
#[path = "../round_trip.rs"]
mod round_trip;
