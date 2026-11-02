//! Every integration test of wheat-git, in one binary: each file under
//! `tests/` is a module here rather than a test binary of its own, since each
//! binary links most of Wheat. `crates/wheat-core/tests/test_binaries.rs` checks
//! that every file is listed.

#[path = "../export.rs"]
mod export;
#[path = "../git_config_guard.rs"]
mod git_config_guard;
#[path = "../import.rs"]
mod import;
#[path = "../import_modes.rs"]
mod import_modes;
#[path = "../journal.rs"]
mod journal;
#[path = "../push.rs"]
mod push;
#[path = "../staged_import.rs"]
mod staged_import;
