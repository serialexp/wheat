//! Every integration test of wheat-cli, in one binary: each file under
//! `tests/` is a module here rather than a test binary of its own, since each
//! binary links most of Wheat. `crates/wheat-core/tests/test_binaries.rs` checks
//! that every file is listed.

#[cfg(target_os = "linux")]
#[path = "../git_team/mod.rs"]
mod git_team;
#[cfg(target_os = "linux")]
#[path = "../remote_support/mod.rs"]
mod remote_support;
#[cfg(target_os = "linux")]
#[path = "../sandbox/mod.rs"]
mod sandbox;
#[cfg(target_os = "linux")]
#[path = "../support/mod.rs"]
mod support;

#[path = "../active_changes.rs"]
mod active_changes;
#[path = "../add_scale.rs"]
mod add_scale;
#[path = "../authoring_metadata.rs"]
mod authoring_metadata;
#[path = "../automation_contract.rs"]
mod automation_contract;
#[path = "../blame.rs"]
mod blame;
#[path = "../broken_pipe.rs"]
mod broken_pipe;
#[path = "../client_authoring.rs"]
mod client_authoring;
#[path = "../client_history.rs"]
mod client_history;
#[path = "../clone_labels.rs"]
mod clone_labels;
#[path = "../conflict_validation.rs"]
mod conflict_validation;
#[path = "../diff_paths.rs"]
mod diff_paths;
#[path = "../dot_git.rs"]
mod dot_git;
#[path = "../edit_move.rs"]
mod edit_move;
#[path = "../git_import.rs"]
mod git_import;
#[path = "../git_large_files.rs"]
mod git_large_files;
#[path = "../git_pull.rs"]
mod git_pull;
#[path = "../git_push.rs"]
mod git_push;
#[path = "../git_push_tag.rs"]
mod git_push_tag;
#[path = "../git_round_trip.rs"]
mod git_round_trip;
#[path = "../git_wide_directory.rs"]
mod git_wide_directory;
#[path = "../hidden_labels.rs"]
mod hidden_labels;
#[path = "../immutable_diff.rs"]
mod immutable_diff;
#[path = "../integrate.rs"]
mod integrate;
#[path = "../label_fetch.rs"]
mod label_fetch;
#[path = "../label_move.rs"]
mod label_move;
#[path = "../label_publish.rs"]
mod label_publish;
#[path = "../labels.rs"]
mod labels;
#[path = "../local_workflow.rs"]
mod local_workflow;
#[path = "../log_path.rs"]
mod log_path;
#[path = "../materialization_recovery.rs"]
mod materialization_recovery;
#[path = "../phase1_acceptance.rs"]
mod phase1_acceptance;
#[path = "../pinned_view.rs"]
mod pinned_view;
#[path = "../publish_hooks.rs"]
mod publish_hooks;
#[path = "../quiesce.rs"]
mod quiesce;
#[path = "../remote_admin.rs"]
mod remote_admin;
#[path = "../remote_auth.rs"]
mod remote_auth;
#[path = "../remote_fetch.rs"]
mod remote_fetch;
#[path = "../remote_fork.rs"]
mod remote_fork;
#[path = "../remote_large_content.rs"]
mod remote_large_content;
#[path = "../remote_publish.rs"]
mod remote_publish;
#[path = "../repack.rs"]
mod repack;
#[path = "../revise_after_reapply.rs"]
mod revise_after_reapply;
#[path = "../span_trace.rs"]
mod span_trace;
#[path = "../squash.rs"]
mod squash;
#[path = "../stat_cache.rs"]
mod stat_cache;
#[path = "../store_analyze.rs"]
mod store_analyze;
#[path = "../unsupported_platform.rs"]
mod unsupported_platform;
#[path = "../untracked_status.rs"]
mod untracked_status;
#[path = "../workspace_add.rs"]
mod workspace_add;
#[path = "../workspace_add_all.rs"]
mod workspace_add_all;
#[path = "../workspace_diff.rs"]
mod workspace_diff;
#[path = "../workspace_registry.rs"]
mod workspace_registry;
#[path = "../workspace_restore.rs"]
mod workspace_restore;
#[path = "../workspace_settlement.rs"]
mod workspace_settlement;
