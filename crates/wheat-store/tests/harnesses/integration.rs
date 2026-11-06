//! Every integration test of wheat-store, in one binary: each file under
//! `tests/` is a module here rather than a test binary of its own, since each
//! binary links most of Wheat. `crates/wheat-core/tests/test_binaries.rs` checks
//! that every file is listed.

#[path = "../network_support/mod.rs"]
mod network_support;
#[path = "../support/mod.rs"]
mod support;

#[path = "../actor_concurrency_corpus.rs"]
mod actor_concurrency_corpus;
#[path = "../authoring_spot.rs"]
mod authoring_spot;
#[path = "../batch_store.rs"]
mod batch_store;
#[path = "../blame.rs"]
mod blame;
#[path = "../branch_publication.rs"]
mod branch_publication;
#[path = "../browse.rs"]
mod browse;
#[path = "../collection_builder_parity.rs"]
mod collection_builder_parity;
#[path = "../collection_failures.rs"]
mod collection_failures;
#[path = "../commit_content.rs"]
mod commit_content;
#[path = "../commit_writer.rs"]
mod commit_writer;
#[path = "../content_batch.rs"]
mod content_batch;
#[path = "../content_blobs.rs"]
mod content_blobs;
#[path = "../content_gc.rs"]
mod content_gc;
#[path = "../content_generation_read_latency.rs"]
mod content_generation_read_latency;
#[path = "../content_map.rs"]
mod content_map;
#[path = "../content_repack.rs"]
mod content_repack;
#[path = "../content_store.rs"]
mod content_store;
#[path = "../content_stream.rs"]
mod content_stream;
#[path = "../final_tree_update.rs"]
mod final_tree_update;
#[path = "../frontier_index.rs"]
mod frontier_index;
#[path = "../full_corpus_compression_diagnostic.rs"]
mod full_corpus_compression_diagnostic;
#[path = "../generated_backend_parity.rs"]
mod generated_backend_parity;
#[path = "../gothab_block_compression.rs"]
mod gothab_block_compression;
#[path = "../guarded_verification.rs"]
mod guarded_verification;
#[path = "../immutable_object_store.rs"]
mod immutable_object_store;
#[path = "../increment_validation.rs"]
mod increment_validation;
#[path = "../label_move.rs"]
mod label_move;
#[path = "../later_edits.rs"]
mod later_edits;
#[path = "../merkle_tree.rs"]
mod merkle_tree;
#[path = "../metadata_compression_diagnostic.rs"]
mod metadata_compression_diagnostic;
#[path = "../metadata_delta_read_latency.rs"]
mod metadata_delta_read_latency;
#[path = "../metadata_delta_repack.rs"]
mod metadata_delta_repack;
#[path = "../network_bundle.rs"]
mod network_bundle;
#[path = "../network_import.rs"]
mod network_import;
#[path = "../object_store_adapter.rs"]
mod object_store_adapter;
#[path = "../object_store_bench.rs"]
mod object_store_bench;
#[path = "../ownership_liveness.rs"]
mod ownership_liveness;
#[path = "../packed_repack.rs"]
mod packed_repack;
#[path = "../packed_store.rs"]
mod packed_store;
#[path = "../packed_stored_bytes.rs"]
mod packed_stored_bytes;
#[path = "../path_history_index.rs"]
mod path_history_index;
#[path = "../phase1_measurement.rs"]
mod phase1_measurement;
#[path = "../projected_commits.rs"]
mod projected_commits;
#[path = "../publication_store.rs"]
mod publication_store;
#[path = "../remote_packed_store.rs"]
mod remote_packed_store;
#[path = "../repack_lineage_diagnostic.rs"]
mod repack_lineage_diagnostic;
#[path = "../repack_memory_profile.rs"]
mod repack_memory_profile;
#[path = "../repositories_directory.rs"]
mod repositories_directory;
#[path = "../repository_directory.rs"]
mod repository_directory;
#[path = "../repository_hydration.rs"]
mod repository_hydration;
#[path = "../repository_integration.rs"]
mod repository_integration;
#[path = "../repository_mutation.rs"]
mod repository_mutation;
#[path = "../repository_mutation_backends.rs"]
mod repository_mutation_backends;
#[path = "../repository_publication.rs"]
mod repository_publication;
#[path = "../repository_rebase.rs"]
mod repository_rebase;
#[path = "../repository_rewrite.rs"]
mod repository_rewrite;
#[path = "../repository_stage.rs"]
mod repository_stage;
#[path = "../repository_union.rs"]
mod repository_union;
#[path = "../retained_set_delta.rs"]
mod retained_set_delta;
#[path = "../revision_map.rs"]
mod revision_map;
#[path = "../root_publication.rs"]
mod root_publication;
#[path = "../sharded_content_generation.rs"]
mod sharded_content_generation;
#[path = "../snapshot_restore.rs"]
mod snapshot_restore;
#[path = "../structural_delta.rs"]
mod structural_delta;
#[path = "../transition_validation.rs"]
mod transition_validation;
#[path = "../two_phase_mutation_publication.rs"]
mod two_phase_mutation_publication;
#[path = "../typed_collections.rs"]
mod typed_collections;
#[path = "../verification_cost.rs"]
mod verification_cost;
#[path = "../verification_paired_walk.rs"]
mod verification_paired_walk;
#[path = "../view_lineage.rs"]
mod view_lineage;
#[path = "../view_order_validation.rs"]
mod view_order_validation;
#[path = "../wide_directories.rs"]
mod wide_directories;
