use std::{
    fs::{File, OpenOptions},
    io::{self, BufWriter, Write},
    path::Path,
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::{Value, json};
use wheat_git::{AtomicImportEvent, AtomicImportPhase, FinalizePhase};
use wheat_store::PackedStoreTraceSnapshot;

const SCHEMA: &str = "wheat.import-trace.v1";

pub struct ImportTrace {
    output: BufWriter<File>,
    started: Instant,
    seq: u64,
    failure: Option<io::Error>,
}

#[derive(Serialize)]
struct Record<'a> {
    schema: &'static str,
    seq: u64,
    elapsed_us: u64,
    event: &'a str,
    data: Value,
}

impl ImportTrace {
    pub fn create(path: &Path) -> io::Result<Self> {
        let output = OpenOptions::new().write(true).create_new(true).open(path)?;
        Ok(Self {
            output: BufWriter::new(output),
            started: Instant::now(),
            seq: 0,
            failure: None,
        })
    }

    pub fn started(&mut self, remote_clone: bool) {
        self.emit(
            "import_started",
            json!({
                "source_kind": if remote_clone { "remote" } else { "local" },
                "remote_clone": remote_clone,
            }),
        );
    }

    pub fn phase_started(&mut self, phase: &str) {
        self.emit("phase_started", json!({ "phase": phase }));
    }

    pub fn phase_finished(&mut self, phase: &str, elapsed: Duration) {
        self.emit(
            "phase_finished",
            json!({ "phase": phase, "duration_us": micros(elapsed) }),
        );
    }

    pub fn atomic_event(&mut self, event: AtomicImportEvent) {
        match event {
            AtomicImportEvent::PhaseStarted { phase } => {
                self.phase_started(&format_atomic_phase(phase));
            }
            AtomicImportEvent::PhaseFinished { phase, elapsed } => {
                self.phase_finished(&format_atomic_phase(phase), elapsed);
            }
            AtomicImportEvent::StageProgress {
                batch,
                elapsed,
                complete,
                completed_commits,
                pending_commits,
                discovery_pending,
                branches,
                storage,
            } => self.emit(
                "stage_progress",
                json!({
                    "batch": batch,
                    "duration_us": micros(elapsed),
                    "complete": complete,
                    "completed_commits": completed_commits,
                    "pending_commits": pending_commits,
                    "discovery_pending": discovery_pending,
                    "branches": branches,
                    "storage": storage_json(&storage),
                }),
            ),
            AtomicImportEvent::FinalizePhaseStarted { phase } => self.emit(
                "finalize_phase_started",
                json!({ "phase": finalize_phase(phase) }),
            ),
            AtomicImportEvent::FinalizePhaseFinished {
                phase,
                elapsed,
                storage,
            } => self.emit(
                "finalize_phase_finished",
                json!({
                    "phase": finalize_phase(phase),
                    "duration_us": micros(elapsed),
                    "storage": storage_json(&storage),
                }),
            ),
            AtomicImportEvent::ValidationPhaseStarted { phase } => self.emit(
                "validation_phase_started",
                json!({ "phase": phase.as_str() }),
            ),
            AtomicImportEvent::ValidationPhaseFinished {
                phase,
                elapsed,
                storage,
            } => self.emit(
                "validation_phase_finished",
                json!({
                    "phase": phase.as_str(),
                    "duration_us": micros(elapsed),
                    "storage": storage_json(&storage),
                }),
            ),
            AtomicImportEvent::StorageClosed { storage } => {
                self.emit(
                    "storage_closed",
                    json!({ "storage": storage_json(&storage) }),
                );
            }
        }
    }

    pub fn error(&mut self, class: &str) {
        self.emit(
            "import_error",
            json!({
                "duration_us": micros(self.started.elapsed()),
                "error_class": class,
            }),
        );
    }

    pub fn complete(&mut self, generation: u64) {
        self.emit(
            "import_complete",
            json!({
                "duration_us": micros(self.started.elapsed()),
                "generation": generation,
            }),
        );
    }

    pub fn finish(mut self) -> io::Result<()> {
        if let Some(error) = self.failure.take() {
            return Err(error);
        }
        self.output.flush()
    }

    fn emit(&mut self, event: &str, data: Value) {
        if self.failure.is_some() {
            return;
        }
        let record = Record {
            schema: SCHEMA,
            seq: self.seq,
            elapsed_us: micros(self.started.elapsed()),
            event,
            data,
        };
        self.seq = self.seq.saturating_add(1);
        let result = serde_json::to_writer(&mut self.output, &record)
            .and_then(|()| self.output.write_all(b"\n").map_err(serde_json::Error::io))
            .and_then(|()| self.output.flush().map_err(serde_json::Error::io));
        if let Err(error) = result {
            self.failure = Some(io::Error::other(error));
        }
    }
}

fn format_atomic_phase(phase: AtomicImportPhase) -> String {
    format!("atomic_import.{}", phase.as_str())
}

const fn finalize_phase(phase: FinalizePhase) -> &'static str {
    phase.as_str()
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn storage_json(value: &PackedStoreTraceSnapshot) -> Value {
    json!({
        "object_gets": value.object_gets,
        "object_get_bytes": value.object_get_bytes,
        "object_put_batches": value.object_put_batches,
        "object_put_attempts": value.object_put_attempts,
        "object_put_bytes": value.object_put_bytes,
        "object_gets_by_kind": value.object_gets_by_kind,
        "content_gets": value.content_gets,
        "content_get_bytes": value.content_get_bytes,
        "content_put_batches": value.content_put_batches,
        "content_put_attempts": value.content_put_attempts,
        "content_put_bytes": value.content_put_bytes,
        "catalog_gets": value.catalog_gets,
        "catalog_bulk_hits": value.catalog_bulk_hits,
        "catalog_locator_hits": value.catalog_locator_hits,
        "deferred_put_calls": value.deferred_put_calls,
        "deferred_put_attempts": value.deferred_put_attempts,
        "deferred_put_duplicates": value.deferred_put_duplicates,
        "deferred_appends": value.deferred_appends,
        "deferred_index_transactions": value.deferred_index_transactions,
        "locator_recounts": value.locator_recounts,
        "bulk_rolls": value.bulk_rolls,
        "bulk_finalizations": value.bulk_finalizations,
        "catalog_lookup_us": value.catalog_lookup_ns / 1_000,
        "deferred_append_us": value.deferred_append_ns / 1_000,
        "deferred_index_transaction_us": value.deferred_index_transaction_ns / 1_000,
        "locator_recount_us": value.locator_recount_ns / 1_000,
        "bulk_finalization_us": value.bulk_finalization_ns / 1_000,
    })
}
