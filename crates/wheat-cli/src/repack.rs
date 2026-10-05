//! CLI orchestration for representation-preserving packed-store repacks.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use wheat_store::{
    LocalPublicationStore, PackedObjectStore, PublicationStore, RepackError,
    RepackOptions as StoreRepackOptions, RepackPlan as StoreRepackPlan,
    RepackReport as StoreRepackReport,
};

use crate::command_output::{CliFailure, Success};
use crate::local::{self, LocalRepository};

const MINIMUM_SAFETY_MARGIN: u64 = 64 * 1024 * 1024;

/// Concrete CLI adapter around the packed store's two-phase repack API.
pub(crate) struct PackedRepackBackend<'a> {
    store_root: PathBuf,
    objects: &'a PackedObjectStore,
    publications: &'a LocalPublicationStore,
}

impl<'a> PackedRepackBackend<'a> {
    pub(crate) fn new(store_root: &Path, repository: &'a LocalRepository) -> Self {
        Self {
            store_root: store_root.to_path_buf(),
            objects: &repository.objects,
            publications: &repository.heads,
        }
    }
}

pub(crate) trait RepackBackend {
    type Plan;

    fn plan_repack(&self) -> Result<(Self::Plan, u64)>;
    fn execute_repack(&self, plan: Self::Plan) -> Result<RepackOutput>;
}

impl RepackBackend for PackedRepackBackend<'_> {
    type Plan = StoreRepackPlan;

    fn plan_repack(&self) -> Result<(Self::Plan, u64)> {
        let plan = local::runtime()?
            .block_on(
                self.objects
                    .plan_repack(self.publications, StoreRepackOptions::default()),
            )
            .map_err(map_repack_error)?;
        let estimate = plan.estimated_temporary_bytes;
        Ok((plan, estimate))
    }

    fn execute_repack(&self, plan: Self::Plan) -> Result<RepackOutput> {
        let head_path = self.store_root.join("repository.head");
        let bytes_before = std::fs::read(&head_path)
            .with_context(|| format!("read {} before repack", head_path.display()))?;
        let head_before = self.publications.read()?;
        let report = self
            .objects
            .execute_repack(self.publications, plan)
            .map_err(map_repack_error)?;
        let bytes_after = std::fs::read(&head_path)
            .with_context(|| format!("read {} after repack", head_path.display()))?;
        let head_after = self.publications.read()?;
        if bytes_before != bytes_after || head_before != head_after {
            bail!(CliFailure::corrupt(
                "repack changed repository.head bytes or logical head"
            ));
        }
        RepackOutput::from_store(report)
    }
}

fn map_repack_error(error: RepackError) -> anyhow::Error {
    match error {
        RepackError::InvalidOptions => CliFailure::invalid(error.to_string()),
        RepackError::InventoryLimit { .. } => CliFailure::limit(error.to_string()),
        RepackError::StaleAuthority => CliFailure::stale(error.to_string()),
        RepackError::Authority(_) | RepackError::Corrupt(_) => {
            CliFailure::corrupt(error.to_string())
        }
        RepackError::Unsupported | RepackError::Content(_) | RepackError::Io(_) => error.into(),
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RepackOutput {
    no_op: bool,
    physical_generation_before: u64,
    physical_generation_after: u64,
    repository_generation: u64,
    envelope_before: String,
    envelope_after: String,
    segments_before: usize,
    segments_after: usize,
    structural_records: u64,
    content_records: u64,
    physical_bytes_before: u64,
    physical_bytes_after: u64,
    full_content_before: u64,
    full_content_after: u64,
    delta_content_before: u64,
    delta_content_after: u64,
    max_delta_depth_before: u8,
    max_delta_depth_after: u8,
    metadata_delta_records: u64,
    max_metadata_delta_depth: u8,
    loose_metadata_absorbed: u64,
    estimated_temporary_bytes: u64,
    available_temporary_bytes: Option<u64>,
    capacity_forced: bool,
    bytes_written: u64,
    bytes_retired: u64,
    validated: bool,
}

impl RepackOutput {
    fn from_store(report: StoreRepackReport) -> Result<Self> {
        if report.envelope_before != report.envelope_after {
            bail!(CliFailure::corrupt(
                "repack changed the repository envelope; logical authority was not preserved"
            ));
        }
        if !report.validated {
            bail!(CliFailure::corrupt(
                "repack completed without validating the replacement physical generation"
            ));
        }
        Ok(Self {
            no_op: report.no_op,
            physical_generation_before: report.physical_generation_before,
            physical_generation_after: report.physical_generation_after,
            repository_generation: report.repository_generation,
            envelope_before: report.envelope_before.to_string(),
            envelope_after: report.envelope_after.to_string(),
            segments_before: report.segments_before,
            segments_after: report.segments_after,
            structural_records: report.structural_record_count,
            content_records: report.content_record_count,
            physical_bytes_before: report.physical_bytes_before,
            physical_bytes_after: report.physical_bytes_after,
            full_content_before: report.full_content_before,
            full_content_after: report.full_content_after,
            delta_content_before: report.delta_content_before,
            delta_content_after: report.delta_content_after,
            max_delta_depth_before: report.max_delta_depth_before,
            max_delta_depth_after: report.max_delta_depth_after,
            metadata_delta_records: report.metadata_delta_records,
            max_metadata_delta_depth: report.max_metadata_delta_depth,
            loose_metadata_absorbed: report.loose_metadata_absorbed,
            estimated_temporary_bytes: report.estimated_temporary_bytes,
            available_temporary_bytes: None,
            capacity_forced: false,
            bytes_written: report.bytes_written,
            bytes_retired: report.bytes_retired,
            validated: report.validated,
        })
    }
}

pub(crate) fn required_temporary_bytes(estimate: u64) -> Result<u64> {
    let safety_margin = (estimate / 10).max(MINIMUM_SAFETY_MARGIN);
    estimate
        .checked_add(safety_margin)
        .ok_or_else(|| CliFailure::limit("repack temporary-space estimate overflowed u64"))
}

pub(crate) fn available_bytes(path: &Path) -> Result<u64> {
    let available = crate::sys::available_bytes(path)
        .context("cannot measure available space for the Wheat store")?;
    u64::try_from(available)
        .map_err(|_| CliFailure::limit("available filesystem capacity overflowed u64"))
}

pub(crate) struct RepackIo<'a> {
    pub(crate) input: &'a mut dyn BufRead,
    pub(crate) diagnostics: &'a mut dyn Write,
    pub(crate) interactive: bool,
}

pub(crate) fn run<B, M>(
    backend: &B,
    force: bool,
    mut measure_available: M,
    io: &mut RepackIo<'_>,
) -> Result<Success>
where
    B: RepackBackend,
    M: FnMut() -> Result<u64>,
{
    let (plan, estimate) = backend.plan_repack()?;
    let required = required_temporary_bytes(estimate)?;
    let available = measure_available().ok();
    let capacity_sufficient = available.is_some_and(|value| value >= required);
    if !force && !capacity_sufficient && (!io.interactive || !confirm(required, available, io)?) {
        bail!(CliFailure::limit(format!(
            "repack capacity preflight failed: {required} temporary bytes required, {} available; rerun with --force to bypass the capacity preflight",
            available.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
        )));
    }

    let mut report = backend.execute_repack(plan)?;
    report.estimated_temporary_bytes = required;
    report.available_temporary_bytes = available;
    report.capacity_forced = force && !capacity_sufficient;
    let human = format_report(&report);
    Success::new("repack", &report, human)
}

fn confirm(required: u64, available: Option<u64>, io: &mut RepackIo<'_>) -> Result<bool> {
    let available = available.map_or_else(|| "unknown".to_owned(), |value| value.to_string());
    write!(
        io.diagnostics,
        "Repack may require {required} temporary bytes, but {available} are available. Proceed with repack? [y/N] "
    )?;
    io.diagnostics.flush()?;
    let mut answer = String::new();
    io.input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn format_report(report: &RepackOutput) -> String {
    let first = if report.no_op {
        format!(
            "nothing to repack; physical generation {} unchanged",
            report.physical_generation_before
        )
    } else {
        format!(
            "repacked physical generation {} -> {}",
            report.physical_generation_before, report.physical_generation_after
        )
    };
    format!(
        "{first}\nsegments {} -> {}; physical bytes {} -> {}\ncontent full/delta {}/{} -> {}/{}; max delta depth {} -> {}\nmetadata deltas {} (max depth {}); loose metadata absorbed {}\nrepository generation {} and envelope {} unchanged; logical objects/content preserved\n",
        report.segments_before,
        report.segments_after,
        report.physical_bytes_before,
        report.physical_bytes_after,
        report.full_content_before,
        report.delta_content_before,
        report.full_content_after,
        report.delta_content_after,
        report.max_delta_depth_before,
        report.max_delta_depth_after,
        report.metadata_delta_records,
        report.max_metadata_delta_depth,
        report.loose_metadata_absorbed,
        report.repository_generation,
        report.envelope_before,
    )
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::io::Cursor;

    use super::*;

    struct FakeBackend {
        estimate: u64,
        report: RefCell<Option<RepackOutput>>,
        executions: Cell<u32>,
    }

    impl RepackBackend for FakeBackend {
        type Plan = ();

        fn plan_repack(&self) -> Result<(Self::Plan, u64)> {
            Ok(((), self.estimate))
        }

        fn execute_repack(&self, (): Self::Plan) -> Result<RepackOutput> {
            self.executions.set(self.executions.get() + 1);
            Ok(self.report.borrow_mut().take().unwrap())
        }
    }

    fn fixture() -> FakeBackend {
        FakeBackend {
            estimate: 600,
            report: RefCell::new(Some(RepackOutput {
                no_op: false,
                physical_generation_before: 4,
                physical_generation_after: 5,
                repository_generation: 9,
                envelope_before: "abc".into(),
                envelope_after: "abc".into(),
                segments_before: 8,
                segments_after: 3,
                structural_records: 11,
                content_records: 12,
                physical_bytes_before: 1_000,
                physical_bytes_after: 700,
                full_content_before: 4,
                full_content_after: 2,
                delta_content_before: 6,
                delta_content_after: 8,
                max_delta_depth_before: 5,
                max_delta_depth_after: 2,
                metadata_delta_records: 7,
                max_metadata_delta_depth: 3,
                loose_metadata_absorbed: 10,
                estimated_temporary_bytes: 0,
                available_temporary_bytes: None,
                capacity_forced: false,
                bytes_written: 700,
                bytes_retired: 1_000,
                validated: true,
            })),
            executions: Cell::new(0),
        }
    }

    fn io<'a>(
        input: &'a mut Cursor<&'static [u8]>,
        output: &'a mut Vec<u8>,
        interactive: bool,
    ) -> RepackIo<'a> {
        RepackIo {
            input,
            diagnostics: output,
            interactive,
        }
    }

    #[test]
    fn capacity_formula_has_minimum_and_proportional_margin() {
        assert_eq!(required_temporary_bytes(6).unwrap(), 6 + 64 * 1024 * 1024);
        assert_eq!(
            required_temporary_bytes(1_000_000_000).unwrap(),
            1_100_000_000
        );
        assert!(required_temporary_bytes(u64::MAX).is_err());
    }

    #[test]
    fn noninteractive_unknown_capacity_fails_without_executing() {
        let backend = fixture();
        let mut input = Cursor::new(b"yes\n" as &'static [u8]);
        let mut diagnostics = Vec::new();
        let error = match run(
            &backend,
            false,
            || bail!("unavailable"),
            &mut io(&mut input, &mut diagnostics, false),
        ) {
            Ok(_) => panic!("unknown noninteractive capacity unexpectedly proceeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("rerun with --force"));
        assert_eq!(backend.executions.get(), 0);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn force_bypasses_only_capacity_gate() {
        let backend = fixture();
        let mut input = Cursor::new(b"" as &'static [u8]);
        let mut diagnostics = Vec::new();
        let success = run(
            &backend,
            true,
            || Ok(0),
            &mut io(&mut input, &mut diagnostics, false),
        )
        .unwrap();
        assert_eq!(success.result["capacity_forced"], true);
        assert_eq!(backend.executions.get(), 1);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn report_shows_metadata_delta_statistics() {
        let report = fixture().report.take().unwrap();
        let text = format_report(&report);
        assert!(
            text.contains("\nmetadata deltas 7 (max depth 3); loose metadata absorbed 10\n"),
            "{text}"
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["metadata_delta_records"], 7);
        assert_eq!(json["max_metadata_delta_depth"], 3);
        assert_eq!(json["loose_metadata_absorbed"], 10);
    }
}
