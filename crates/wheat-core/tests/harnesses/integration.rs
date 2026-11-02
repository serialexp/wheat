//! Every integration test of wheat-core, in one binary: each file under
//! `tests/` is a module here rather than a test binary of its own, since each
//! binary links most of Wheat. `crates/wheat-core/tests/test_binaries.rs` checks
//! that every file is listed.

#[path = "../application_order.rs"]
mod application_order;
#[path = "../branch_records.rs"]
mod branch_records;
#[path = "../canonical_collections.rs"]
mod canonical_collections;
#[path = "../canonical_objects.rs"]
mod canonical_objects;
#[path = "../directory_chunks.rs"]
mod directory_chunks;
#[path = "../edit_application.rs"]
mod edit_application;
#[path = "../edit_application_failures.rs"]
mod edit_application_failures;
#[path = "../edit_capture.rs"]
mod edit_capture;
#[path = "../edit_records.rs"]
mod edit_records;
#[path = "../forged_lengths.rs"]
mod forged_lengths;
#[path = "../generated_contracts.rs"]
mod generated_contracts;
#[path = "../guarded_authoring.rs"]
mod guarded_authoring;
#[path = "../guarded_records.rs"]
mod guarded_records;
#[path = "../guarded_repository.rs"]
mod guarded_repository;
#[path = "../hydration.rs"]
mod hydration;
#[path = "../no_hand_rolled_encodings.rs"]
mod no_hand_rolled_encodings;
#[path = "../persisted_records.rs"]
mod persisted_records;
#[path = "../repository_closure.rs"]
mod repository_closure;
#[path = "../segment_records.rs"]
mod segment_records;
#[path = "../store_records.rs"]
mod store_records;
#[path = "../test_binaries.rs"]
mod test_binaries;
#[path = "../workflow.rs"]
mod workflow;
