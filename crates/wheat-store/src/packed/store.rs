//! Public object/content adapters backed by immutable packed segments.
use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;
use thiserror::Error;
use wheat_core::{
    ContentId, ObjectId, PhysicalContentGeneration, RepositoryEnvelopeId, validate_canonical_object,
};

use super::{
    catalog::{CatalogError, PackedCatalog},
    generation_reader::GenerationReader,
    segment::{CONTENT_NAMESPACE, METADATA_NAMESPACE, SegmentRecord},
    trace::{PackedStoreTrace, PackedStoreTraceCounters},
};
use crate::content_store::ContentBackend;
use crate::{
    ContentGenerationManifest, ContentRepackLimits, ContentRepackPlan, ContentStore,
    ContentStoreError, Durability, ImmutableObjectStore, LocalPublicationStore, LooseObjectStore,
    ObjectKind, ObjectStoreError, PublicationStore, RepositoryHead, RepositoryReader, StoredObject,
    content_repack::{plan_content_repack, prepare_content_repack, retained_lineages},
};

const DEFAULT_LOOSE_THRESHOLD: usize = 128 * 1024;
const DEFAULT_REPACK_MAX_RECORDS: usize = 1_000_000;
const DEFAULT_REPACK_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct RepackOptions {
    /// Hard bound on authenticated packed records inventoried in one run.
    pub max_records: usize,
    /// Approximate uncompressed logical bytes per replacement segment.
    pub target_segment_bytes: u64,
    pub content_limits: ContentRepackLimits,
}

impl Default for RepackOptions {
    fn default() -> Self {
        Self {
            max_records: DEFAULT_REPACK_MAX_RECORDS,
            target_segment_bytes: DEFAULT_REPACK_SEGMENT_BYTES,
            content_limits: ContentRepackLimits::default(),
        }
    }
}

#[derive(Debug, Error)]
pub enum RepackError {
    #[error("repack options require non-zero limits")]
    InvalidOptions,
    #[error("repack is unsupported for deferred storage or pending maintenance")]
    Unsupported,
    #[error("repack inventory exceeds its {limit} record limit")]
    InventoryLimit { limit: usize },
    #[error("repack plan is stale; logical or physical authority changed")]
    StaleAuthority,
    #[error("repack repository authority failed: {0}")]
    Authority(String),
    #[error(transparent)]
    Content(#[from] ContentStoreError),
    #[error("repack authority is corrupt: {0}")]
    Corrupt(String),
    #[error("repack I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug)]
pub struct RepackPlan {
    physical_authority: super::catalog::CatalogPhysicalAuthority,
    repository_head: RepositoryHead,
    options: RepackOptions,
    prepared_content: ContentRepackPlan,
    content_before: Option<ContentGenerationManifest>,
    pub estimated_replacement_structural_bytes: u64,
    pub estimated_replacement_content_bytes: u64,
    pub estimated_locator_manifest_bytes: u64,
    pub estimated_temporary_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepackReport {
    pub no_op: bool,
    pub physical_generation_before: u64,
    pub physical_generation_after: u64,
    pub segments_before: usize,
    pub segments_after: usize,
    pub physical_bytes_before: u64,
    pub physical_bytes_after: u64,
    pub repository_generation: u64,
    pub envelope_before: RepositoryEnvelopeId,
    pub envelope_after: RepositoryEnvelopeId,
    pub structural_record_count: u64,
    pub content_record_count: u64,
    pub full_content_before: u64,
    pub full_content_after: u64,
    pub delta_content_before: u64,
    pub delta_content_after: u64,
    pub max_delta_depth_before: u8,
    pub max_delta_depth_after: u8,
    pub estimated_temporary_bytes: u64,
    pub bytes_written: u64,
    pub bytes_retired: u64,
    pub validated: bool,
}

#[derive(Clone)]
pub struct PackedObjectStore {
    engine: Arc<PackedEngine>,
}

struct PackedEngine {
    catalog: Arc<Mutex<PackedCatalog>>,
    generation_reader: Mutex<Option<GenerationReader>>,
    objects: LooseObjectStore,
    content: ContentStore,
    root: PathBuf,
    content_root: PathBuf,
    loose_threshold: usize,
    durability: Durability,
    trace: Option<Arc<PackedStoreTraceCounters>>,
}

impl fmt::Debug for PackedObjectStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PackedObjectStore")
            .field("loose_threshold", &self.engine.loose_threshold)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for PackedEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PackedEngine")
            .field("loose_threshold", &self.loose_threshold)
            .finish_non_exhaustive()
    }
}

impl PackedObjectStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ObjectStoreError> {
        Self::open_with_options(root, DEFAULT_LOOSE_THRESHOLD, Durability::Durable, false)
    }

    /// Opens storage for an isolated transaction whose entire directory remains
    /// invisible until [`crate::sync_filesystem`] and atomic installation.
    pub fn open_deferred(root: impl AsRef<Path>) -> Result<Self, ObjectStoreError> {
        Self::open_with_options(root, DEFAULT_LOOSE_THRESHOLD, Durability::Deferred, false)
    }

    /// Opens deferred storage with constant-memory diagnostic counters enabled.
    pub fn open_deferred_traced(
        root: impl AsRef<Path>,
    ) -> Result<(Self, PackedStoreTrace), ObjectStoreError> {
        let store =
            Self::open_with_options(root, DEFAULT_LOOSE_THRESHOLD, Durability::Deferred, true)?;
        let trace = PackedStoreTrace {
            counters: Arc::downgrade(store.engine.trace.as_ref().expect("trace was enabled")),
        };
        Ok((store, trace))
    }

    pub fn open_with_loose_threshold(
        root: impl AsRef<Path>,
        loose_threshold: usize,
    ) -> Result<Self, ObjectStoreError> {
        Self::open_with_options(root, loose_threshold, Durability::Durable, false)
    }

    fn open_with_options(
        root: impl AsRef<Path>,
        loose_threshold: usize,
        durability: Durability,
        collect_trace: bool,
    ) -> Result<Self, ObjectStoreError> {
        let root = root.as_ref();
        let trace = collect_trace.then(|| Arc::new(PackedStoreTraceCounters::default()));
        let catalog = if let Some(trace) = trace.clone() {
            PackedCatalog::open_with_trace(root.join("packed"), durability, trace)
        } else {
            PackedCatalog::open(root.join("packed"), durability)
        }
        .map_err(object_catalog_error)?;
        validate_content_generation(root, catalog.content_generation())
            .map_err(|error| ObjectStoreError::Integrity(error.to_string()))?;
        Ok(Self {
            engine: Arc::new(PackedEngine {
                catalog: Arc::new(Mutex::new(catalog)),
                generation_reader: Mutex::new(None),
                objects: LooseObjectStore::with_durability(root.join("loose-objects"), durability),
                content: ContentStore::with_durability(root.join("loose-content"), durability),
                root: root.to_path_buf(),
                content_root: root.join("loose-content"),
                loose_threshold,
                durability,
                trace,
            }),
        })
    }

    #[must_use]
    pub fn content_store(&self) -> ContentStore {
        ContentStore::with_backend(
            self.engine.content_root.clone(),
            self.engine.clone(),
            self.engine.durability,
        )
    }

    /// Explicitly closes every packed-store file handle and releases its process lock.
    ///
    /// Closing consumes this handle and fails while another object-store clone or a content
    /// adapter still references the same engine. Callers performing a terminal filesystem sync
    /// must drop those handles first so close-time SQLite errors are observable before the sync.
    #[cfg(test)]
    pub(crate) fn test_fail_close_before_finalize(&self) -> Result<(), ObjectStoreError> {
        self.catalog()?
            .test_set_close_failpoint(super::catalog::CloseFailpoint::BeforeFinalize);
        Ok(())
    }

    pub fn close(self) -> Result<(), ObjectStoreError> {
        self.close_with_trace().map(|_| ())
    }

    /// Closes the store and returns its final trace snapshot when tracing was enabled.
    pub fn close_with_trace(
        self,
    ) -> Result<Option<super::PackedStoreTraceSnapshot>, ObjectStoreError> {
        let engine = Arc::try_unwrap(self.engine).map_err(|_| {
            ObjectStoreError::Backend(
                "StillReferenced: packed object/content store handles remain live".into(),
            )
        })?;
        let catalog = Arc::try_unwrap(engine.catalog)
            .map_err(|_| {
                ObjectStoreError::Backend(
                    "StillReferenced: packed catalog handle remains live".into(),
                )
            })?
            .into_inner()
            .map_err(|_| ObjectStoreError::Backend("packed catalog mutex poisoned".into()))?;
        catalog.close().map_err(object_catalog_error)?;
        Ok(engine.trace.map(|trace| trace.snapshot()))
    }

    /// Plan a whole-repository repack against exact authenticated logical and
    /// physical authority. The returned plan is intentionally single-use.
    pub async fn plan_repack(
        &self,
        publications: &impl PublicationStore,
        options: RepackOptions,
    ) -> Result<RepackPlan, RepackError> {
        if options.max_records == 0 || options.target_segment_bytes == 0 {
            return Err(RepackError::InvalidOptions);
        }
        let repository_head = publications
            .read()
            .map_err(|error| RepackError::Authority(error.to_string()))?;
        let content = self.content_store();
        let reader = RepositoryReader::new(self, &content);
        let envelope = reader
            .validate_envelope(repository_head.envelope)
            .await
            .map_err(|error| RepackError::Authority(error.to_string()))?;
        let repository = reader
            .hydrate(envelope.state)
            .await
            .map_err(|error| RepackError::Authority(error.to_string()))?;
        let (physical_authority, segments, physical_bytes, records, mut inventory) = {
            let mut catalog = self
                .catalog()
                .map_err(|error| RepackError::Corrupt(error.to_string()))?;
            let authority = catalog.physical_authority().map_err(repack_catalog_error)?;
            let (segments, bytes, records) =
                catalog.physical_stats().map_err(repack_catalog_error)?;
            let inventory = catalog.packed_content_ids().map_err(repack_catalog_error)?;
            (authority, segments, bytes, records, inventory)
        };
        let loose_ids = list_loose_content_ids(&self.engine.content_root)?;
        for id in &loose_ids {
            crate::content_store::resolve_delta_chain(
                *id,
                options.content_limits.max_record_logical_bytes,
                |current| {
                    if current == *id {
                        self.engine.content.read_local_raw_envelope(current)
                    } else {
                        self.engine.get_raw_envelope(current)
                    }
                },
            )?;
        }
        inventory.extend(loose_ids);
        let current_manifest =
            read_content_manifest(&self.engine.root, physical_authority.content)?;
        // The selected generation may be the sole physical home of an ID after
        // redundant legacy copies are retired. Its authenticated inventory is
        // authoritative for every subsequent repack, not only this generation.
        if let Some(manifest) = &current_manifest {
            inventory.extend(manifest.entries.iter().map(|entry| entry.id));
        }
        inventory.sort_unstable();
        inventory.dedup();
        let lineages = retained_lineages(&repository);
        let prepared_content = plan_content_repack(
            &self.content_store(),
            current_manifest.as_ref(),
            crate::ContentRepackInput {
                content_ids: inventory,
                lineages_newest_first: lineages,
            },
            options.content_limits,
        )
        .map_err(|error| RepackError::Corrupt(error.to_string()))?;
        let content_bytes = match &prepared_content {
            ContentRepackPlan::NoOp { .. } => 0,
            ContentRepackPlan::Replace(analysis) => analysis.estimated_bytes(),
            ContentRepackPlan::ReplacePrepared(_) => {
                return Err(RepackError::Corrupt(
                    "prepared content appeared during planning".into(),
                ));
            }
        };
        let structural_bytes = physical_bytes;
        let locator_bytes = records
            .checked_mul(80)
            .ok_or_else(|| RepackError::Corrupt("locator estimate overflow".into()))?;
        let temporary = structural_bytes
            .checked_add(content_bytes)
            .and_then(|value| value.checked_add(locator_bytes))
            .ok_or_else(|| RepackError::Corrupt("temporary estimate overflow".into()))?;
        let _ = segments;
        Ok(RepackPlan {
            physical_authority,
            repository_head,
            options,
            prepared_content,
            content_before: current_manifest,
            estimated_replacement_structural_bytes: structural_bytes,
            estimated_replacement_content_bytes: content_bytes,
            estimated_locator_manifest_bytes: locator_bytes,
            estimated_temporary_bytes: temporary,
        })
    }

    pub fn execute_repack(
        &self,
        publications: &LocalPublicationStore,
        plan: RepackPlan,
    ) -> Result<RepackReport, RepackError> {
        publications.with_repack_authority(plan.repository_head, || self.execute_bound_repack(plan))
    }

    fn execute_bound_repack(&self, mut plan: RepackPlan) -> Result<RepackReport, RepackError> {
        // Bind authority before expensive generation, then generate/stage only
        // after the caller's capacity gate has admitted execution.
        {
            let catalog = self
                .catalog()
                .map_err(|error| RepackError::Corrupt(error.to_string()))?;
            if catalog.physical_authority().map_err(repack_catalog_error)?
                != plan.physical_authority
            {
                return Err(RepackError::StaleAuthority);
            }
        }
        if let ContentRepackPlan::Replace(analysis) = &plan.prepared_content {
            let prepared = prepare_content_repack(&self.content_store(), analysis)
                .map_err(|error| RepackError::Corrupt(error.to_string()))?;
            plan.prepared_content = ContentRepackPlan::ReplacePrepared(prepared);
        }
        let (replacement_content, content_after) =
            stage_content_generation(&self.engine.root, &plan)?;
        let mut catalog = self
            .catalog()
            .map_err(|error| RepackError::Corrupt(error.to_string()))?;
        let report = match catalog.repack_structural(
            super::catalog::CatalogRepackOptions {
                max_records: plan.options.max_records,
                target_segment_bytes: plan.options.target_segment_bytes,
            },
            replacement_content,
            plan.physical_authority,
        ) {
            Ok(report) => report,
            Err(error) => {
                drop(catalog);
                // Stale authority is known to precede combined-head publication;
                // remove only this authenticated staged generation. Other errors
                // may be post-commit interruption and are recovered on reopen.
                if matches!(error, CatalogError::RepackStaleAuthority) {
                    remove_staged_content_generation(&self.engine.root, replacement_content)?;
                }
                return Err(repack_catalog_error(error));
            }
        };
        retire_old_content_generations(&self.engine.root, replacement_content.generation)?;
        let empty_before = ContentGenerationManifest {
            generation: 0,
            entries: Vec::new(),
            content_fingerprint: [0; 32],
            manifest_digest: [0; 32],
        };
        let before = plan.content_before.as_ref().unwrap_or(&empty_before);
        let (full_before, delta_before, depth_before) = content_stats(before)?;
        let (full_after, delta_after, depth_after) = content_stats(&content_after)?;
        let content_bytes_before = sum_content_bytes(before)?;
        let content_bytes_after = sum_content_bytes(&content_after)?;
        Ok(RepackReport {
            no_op: report.no_op,
            physical_generation_before: report.generation_before,
            physical_generation_after: report.generation_after,
            segments_before: report.segments_before,
            segments_after: report.segments_after,
            physical_bytes_before: report
                .physical_bytes_before
                .checked_add(content_bytes_before)
                .ok_or_else(|| RepackError::Corrupt("report byte count overflow".into()))?,
            physical_bytes_after: report
                .physical_bytes_after
                .checked_add(content_bytes_after)
                .ok_or_else(|| RepackError::Corrupt("report byte count overflow".into()))?,
            repository_generation: plan.repository_head.generation,
            envelope_before: plan.repository_head.envelope,
            envelope_after: plan.repository_head.envelope,
            structural_record_count: report.structural_records,
            content_record_count: u64::try_from(content_after.entries.len())
                .map_err(|_| RepackError::Corrupt("report record count overflow".into()))?,
            full_content_before: full_before,
            full_content_after: full_after,
            delta_content_before: delta_before,
            delta_content_after: delta_after,
            max_delta_depth_before: depth_before,
            max_delta_depth_after: depth_after,
            estimated_temporary_bytes: plan.estimated_temporary_bytes,
            bytes_written: report
                .bytes_written
                .checked_add(plan.estimated_replacement_content_bytes)
                .ok_or_else(|| RepackError::Corrupt("report written byte count overflow".into()))?,
            bytes_retired: report
                .bytes_retired
                .checked_add(content_bytes_before)
                .ok_or_else(|| RepackError::Corrupt("report retired byte count overflow".into()))?,
            validated: true,
        })
    }

    fn catalog(&self) -> Result<MutexGuard<'_, PackedCatalog>, ObjectStoreError> {
        self.engine
            .catalog
            .lock()
            .map_err(|_| ObjectStoreError::Backend("packed catalog mutex poisoned".into()))
    }
}

fn repack_catalog_error(error: CatalogError) -> RepackError {
    match error {
        CatalogError::Io(error) => RepackError::Io(error),
        CatalogError::RepackUnsupported | CatalogError::Poisoned => RepackError::Unsupported,
        CatalogError::RepackInventoryLimit { limit } => RepackError::InventoryLimit { limit },
        CatalogError::RepackStaleAuthority => RepackError::StaleAuthority,
        error => RepackError::Corrupt(error.to_string()),
    }
}

fn list_loose_content_ids(root: &Path) -> Result<Vec<ContentId>, RepackError> {
    let mut ids = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ids),
        Err(error) => return Err(error.into()),
    };
    for prefix in entries {
        let prefix = prefix?;
        if !prefix.file_type()?.is_dir() {
            continue;
        }
        let prefix_name = prefix.file_name().to_string_lossy().into_owned();
        if prefix_name.len() != 2 {
            continue;
        }
        for entry in fs::read_dir(prefix.path())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let encoded = format!("{prefix_name}{}", entry.file_name().to_string_lossy());
            if let Ok(id) = encoded.parse() {
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

fn validate_content_generation(
    root: &Path,
    generation: PhysicalContentGeneration,
) -> Result<(), RepackError> {
    let Some(manifest) = read_content_manifest(root, generation)? else {
        return Ok(());
    };
    let generation_dir = root
        .join("content-generations")
        .join(generation.generation.to_string());
    let pack_length = fs::metadata(generation_dir.join("content.pack"))
        .map_err(|error| RepackError::Corrupt(format!("content generation pack: {error}")))?
        .len();
    let expected_length = manifest.entries.iter().try_fold(0u64, |end, entry| {
        entry
            .pack_offset
            .checked_add(entry.physical_length)
            .map(|entry_end| end.max(entry_end))
            .ok_or_else(|| RepackError::Corrupt("content pack bounds overflow".into()))
    })?;
    if pack_length != expected_length {
        return Err(RepackError::Corrupt(
            "content generation pack length mismatch".into(),
        ));
    }
    Ok(())
}

pub(super) fn read_content_manifest(
    root: &Path,
    generation: PhysicalContentGeneration,
) -> Result<Option<ContentGenerationManifest>, RepackError> {
    if generation.generation == 0 && generation.manifest == [0; 32] {
        return Ok(None);
    }
    let path = root.join("content-generations").join(format!(
        "{}-{}.manifest",
        generation.generation,
        hex::encode(generation.manifest)
    ));
    let encoded = fs::read(path)?;
    if blake3::hash(&encoded).as_bytes() != &generation.manifest {
        return Err(RepackError::Corrupt(
            "content manifest digest mismatch".into(),
        ));
    }
    ContentGenerationManifest::decode(&encoded)
        .map(Some)
        .map_err(|error| RepackError::Corrupt(error.to_string()))
}

fn stage_content_generation(
    root: &Path,
    plan: &RepackPlan,
) -> Result<(PhysicalContentGeneration, ContentGenerationManifest), RepackError> {
    match &plan.prepared_content {
        ContentRepackPlan::NoOp { .. } => {
            let current = plan.content_before.as_ref().ok_or_else(|| {
                RepackError::Corrupt("legacy content cannot produce a no-op content plan".into())
            })?;
            Ok((
                PhysicalContentGeneration {
                    generation: current.generation,
                    manifest: *blake3::hash(
                        &current
                            .encoded()
                            .map_err(|e| RepackError::Corrupt(e.to_string()))?,
                    )
                    .as_bytes(),
                },
                current.clone(),
            ))
        }
        ContentRepackPlan::Replace(_) => Err(RepackError::Corrupt(
            "content analysis was not executed before staging".into(),
        )),
        ContentRepackPlan::ReplacePrepared(prepared) => {
            prepared
                .manifest
                .verify()
                .map_err(|e| RepackError::Corrupt(e.to_string()))?;
            let directory = root.join("content-generations");
            fs::create_dir_all(&directory)?;
            remove_partial_generation(&directory, prepared.manifest.generation)?;
            let generation_dir = directory.join(prepared.manifest.generation.to_string());
            fs::create_dir_all(&generation_dir)?;
            let mut pack = File::create(generation_dir.join("content.pack"))?;
            let mut pack_offset = 0u64;
            for envelope in prepared
                .envelopes()
                .map_err(|e| RepackError::Corrupt(e.to_string()))?
            {
                let envelope = envelope.map_err(|e| RepackError::Corrupt(e.to_string()))?;
                let entry = prepared
                    .manifest
                    .entries
                    .binary_search_by_key(&envelope.id, |entry| entry.id)
                    .ok()
                    .map(|index| &prepared.manifest.entries[index])
                    .ok_or_else(|| {
                        RepackError::Corrupt("staged content ID is absent from manifest".into())
                    })?;
                if entry.pack_offset != pack_offset
                    || u64::try_from(envelope.bytes.len()).ok() != Some(entry.physical_length)
                    || blake3::hash(&envelope.bytes).as_bytes() != &entry.envelope_digest
                {
                    return Err(RepackError::Corrupt(
                        "staged content envelope differs from manifest".into(),
                    ));
                }
                pack.write_all(&envelope.bytes)?;
                pack_offset = pack_offset
                    .checked_add(entry.physical_length)
                    .ok_or_else(|| RepackError::Corrupt("content pack offset overflow".into()))?;
            }
            // One data-file fsync and one directory fsync make the complete pack
            // durable; durability work does not scale with content item count.
            pack.sync_all()?;
            File::open(&generation_dir)?.sync_all()?;
            let encoded = prepared
                .manifest
                .encoded()
                .map_err(|e| RepackError::Corrupt(e.to_string()))?;
            let digest = *blake3::hash(&encoded).as_bytes();
            let manifest_path = directory.join(format!(
                "{}-{}.manifest",
                prepared.manifest.generation,
                hex::encode(digest)
            ));
            let mut file = File::create(manifest_path)?;
            file.write_all(&encoded)?;
            file.sync_all()?;
            File::open(&directory)?.sync_all()?;
            Ok((
                PhysicalContentGeneration {
                    generation: prepared.manifest.generation,
                    manifest: digest,
                },
                prepared.manifest.clone(),
            ))
        }
    }
}

fn authenticated_generation_dir(directory: &Path, generation: u64) -> Result<bool, RepackError> {
    let path = directory.join(generation.to_string());
    if !path.is_dir() || !path.join("content.pack").is_file() {
        return Ok(false);
    }
    let prefix = format!("{generation}-");
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(digest_text) = name
            .strip_prefix(&prefix)
            .and_then(|name| name.strip_suffix(".manifest"))
        else {
            continue;
        };
        if digest_text.len() != 64 || !digest_text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let encoded = fs::read(entry.path())?;
        if hex::encode(blake3::hash(&encoded).as_bytes()) == digest_text
            && ContentGenerationManifest::decode(&encoded)
                .is_ok_and(|manifest| manifest.generation == generation)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn remove_partial_generation(directory: &Path, generation: u64) -> Result<(), RepackError> {
    let path = directory.join(generation.to_string());
    if path.exists() {
        if !authenticated_generation_dir(directory, generation)? {
            return Err(RepackError::Corrupt(format!(
                "refusing to delete unauthenticated content generation {generation}"
            )));
        }
        fs::remove_dir_all(&path)?;
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

fn remove_staged_content_generation(
    root: &Path,
    generation: PhysicalContentGeneration,
) -> Result<(), RepackError> {
    let directory = root.join("content-generations");
    if generation.generation != 0
        && read_content_manifest(root, generation)?.is_some()
        && authenticated_generation_dir(&directory, generation.generation)?
    {
        fs::remove_dir_all(directory.join(generation.generation.to_string()))?;
        let manifest = directory.join(format!(
            "{}-{}.manifest",
            generation.generation,
            hex::encode(generation.manifest)
        ));
        if manifest.exists() {
            fs::remove_file(manifest)?;
        }
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

fn retire_old_content_generations(root: &Path, retained: u64) -> Result<(), RepackError> {
    let directory = root.join("content-generations");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(generation) = name.parse::<u64>() else {
            continue;
        };
        if generation != retained && authenticated_generation_dir(&directory, generation)? {
            fs::remove_dir_all(entry.path())?;
        }
    }
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn sum_content_bytes(manifest: &ContentGenerationManifest) -> Result<u64, RepackError> {
    manifest.entries.iter().try_fold(0u64, |sum, entry| {
        sum.checked_add(entry.physical_length)
            .ok_or_else(|| RepackError::Corrupt("content byte count overflow".into()))
    })
}

fn content_stats(manifest: &ContentGenerationManifest) -> Result<(u64, u64, u8), RepackError> {
    let delta = u64::try_from(
        manifest
            .entries
            .iter()
            .filter(|entry| entry.delta_base.is_some())
            .count(),
    )
    .map_err(|_| RepackError::Corrupt("content count overflow".into()))?;
    let total = u64::try_from(manifest.entries.len())
        .map_err(|_| RepackError::Corrupt("content count overflow".into()))?;
    let full = total
        .checked_sub(delta)
        .ok_or_else(|| RepackError::Corrupt("content count underflow".into()))?;
    Ok((
        full,
        delta,
        manifest
            .entries
            .iter()
            .map(|entry| entry.delta_depth)
            .max()
            .unwrap_or(0),
    ))
}

fn object_catalog_error(error: CatalogError) -> ObjectStoreError {
    match error {
        CatalogError::Io(error) => ObjectStoreError::Io(error),
        CatalogError::Database(error) => ObjectStoreError::Backend(error.to_string()),
        error => ObjectStoreError::Integrity(error.to_string()),
    }
}
fn content_catalog_error(id: ContentId, error: CatalogError) -> ContentStoreError {
    match error {
        CatalogError::Io(error) => ContentStoreError::Io(error),
        CatalogError::Database(error) => ContentStoreError::Io(io::Error::other(error.to_string())),
        error => ContentStoreError::InvalidFormat {
            id,
            reason: error.to_string(),
        },
    }
}

#[async_trait]
impl ImmutableObjectStore for PackedObjectStore {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        self.put_batch(&[(id, bytes.to_vec())], 1).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        if let Some(trace) = &self.engine.trace {
            trace.object_put_batch(
                objects.len(),
                objects.iter().map(|(_, bytes)| bytes.len()).sum(),
            );
        }
        let mut unique = BTreeMap::new();
        for (id, bytes) in objects {
            let canonical_kind: ObjectKind = validate_canonical_object(bytes)?.into();
            let actual = ObjectId::of_canonical_bytes(bytes);
            if actual != *id {
                return Err(ObjectStoreError::Corrupt {
                    expected: *id,
                    actual,
                });
            }
            if let Some(previous) = unique.insert(*id, (canonical_kind, bytes))
                && previous.1 != bytes
            {
                return Err(ObjectStoreError::Corrupt {
                    expected: *id,
                    actual,
                });
            }
        }
        let mut packed = Vec::new();
        let mut loose = Vec::new();
        for (id, (kind, bytes)) in unique {
            if bytes.len() > self.engine.loose_threshold {
                loose.push((id, bytes.clone()));
            } else {
                packed.push((id, kind, bytes));
            }
        }
        if !loose.is_empty() {
            self.engine.objects.put_batch(&loose, concurrency).await?;
        }
        if !packed.is_empty() {
            let records = packed
                .iter()
                .map(|(id, kind, data)| SegmentRecord {
                    namespace: METADATA_NAMESPACE,
                    kind: *kind as u16,
                    digest: *id.as_bytes(),
                    bytes: data,
                })
                .collect::<Vec<_>>();
            self.catalog()?
                .put_records(&records)
                .map_err(object_catalog_error)?;
        }
        Ok(())
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        let Some((stored_kind, bytes)) = self
            .catalog()?
            .get(METADATA_NAMESPACE, *id.as_bytes())
            .map_err(object_catalog_error)?
        else {
            // Oversized metadata bypasses packed segments. Probe the loose
            // fallback only after the in-memory locator says the ID is not packed;
            // the common packed path must not issue one failed filesystem lookup
            // per logical object read.
            let object = self.engine.objects.get(id).await?;
            if let Some(trace) = &self.engine.trace {
                trace.object_get(object.kind as u8, object.canonical_bytes.len());
            }
            return Ok(object);
        };
        // SegmentReader authenticated the immutable segment and verified this
        // payload against the requested object digest before returning it.
        let kind = ObjectKind::from_code(stored_kind).ok_or_else(|| {
            ObjectStoreError::Integrity("packed metadata has an unknown object kind".into())
        })?;
        if let Some(trace) = &self.engine.trace {
            trace.object_get(kind as u8, bytes.len());
        }
        Ok(StoredObject {
            id,
            kind,
            canonical_bytes: bytes.into(),
        })
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            result.push(match self.get(*id).await {
                Ok(_) => true,
                Err(ObjectStoreError::Missing(_)) => false,
                Err(e) => return Err(e),
            });
        }
        Ok(result)
    }
}

#[async_trait]
impl ContentBackend for PackedEngine {
    fn put_many(&self, contents: &[(ContentId, Vec<u8>)]) -> Result<(), ContentStoreError> {
        if let Some(trace) = &self.trace {
            trace.content_put_batch(
                contents.len(),
                contents.iter().map(|(_, bytes)| bytes.len()).sum(),
            );
        }
        let mut packed = Vec::new();
        for (id, bytes) in contents {
            let actual = ContentId::of(bytes);
            if actual != *id {
                return Err(ContentStoreError::Corrupt {
                    expected: *id,
                    actual,
                });
            }
            if bytes.len() > self.loose_threshold {
                self.content.put_many([(*id, bytes.clone())], 1)?;
            } else {
                packed.push((*id, bytes));
            }
        }
        if !packed.is_empty() {
            let records = packed
                .iter()
                .map(|(id, bytes)| SegmentRecord {
                    namespace: CONTENT_NAMESPACE,
                    kind: 0,
                    digest: *id.as_bytes(),
                    bytes,
                })
                .collect::<Vec<_>>();
            self.catalog
                .lock()
                .map_err(|_| {
                    ContentStoreError::Io(io::Error::other("packed catalog mutex poisoned"))
                })?
                .put_records(&records)
                .map_err(|error| content_catalog_error(packed[0].0, error))?;
        }
        Ok(())
    }

    fn get_bounded(&self, id: ContentId, max: usize) -> Result<Vec<u8>, ContentStoreError> {
        let generation = self
            .catalog
            .lock()
            .map_err(|_| ContentStoreError::Io(io::Error::other("packed catalog mutex poisoned")))?
            .content_generation();
        if generation.generation != 0 || generation.manifest != [0; 32] {
            return crate::content_store::resolve_delta_chain(id, max, |current_id| {
                self.get_raw_envelope(current_id)
            });
        }
        let value = self
            .catalog
            .lock()
            .map_err(|_| ContentStoreError::Io(io::Error::other("packed catalog mutex poisoned")))?
            .get_bounded(CONTENT_NAMESPACE, *id.as_bytes(), max)
            .map_err(|error| match error {
                CatalogError::Segment(super::segment::SegmentError::ReadLimit { minimum }) => {
                    ContentStoreError::TooLarge { id, max, minimum }
                }
                error => content_catalog_error(id, error),
            })?;
        if let Some((_, bytes)) = value.as_ref().filter(|(_, bytes)| bytes.len() > max) {
            return Err(ContentStoreError::TooLarge {
                id,
                max,
                minimum: bytes.len(),
            });
        }
        let Some((_kind, bytes)) = value else {
            // Not in a packed segment — could be oversized raw content or a
            // delta envelope in the loose content directory. Use
            // resolve_delta_chain with our own get_raw_envelope so that a
            // delta stored loose can reference a base in a packed segment.
            let bytes = crate::content_store::resolve_delta_chain(id, max, |current_id| {
                self.get_raw_envelope(current_id)
            })?;
            if let Some(trace) = &self.trace {
                trace.content_get(bytes.len());
            }
            return Ok(bytes);
        };
        // SegmentReader authenticated the immutable segment and verified this
        // payload against the requested content digest before returning it.
        if let Some(trace) = &self.trace {
            trace.content_get(bytes.len());
        }
        Ok(bytes)
    }

    fn get_raw_envelope(
        &self,
        id: ContentId,
    ) -> Result<(crate::codec::EnvelopeHeader, Vec<u8>), ContentStoreError> {
        // Hold the catalog lock through the generation lookup so a repack on this
        // handle cannot retire its pack before this read completes.
        let catalog = self.catalog.lock().map_err(|_| {
            ContentStoreError::Io(io::Error::other("packed catalog mutex poisoned"))
        })?;
        let generation = catalog.content_generation();
        if generation.generation != 0 || generation.manifest != [0; 32] {
            let mut cached = self.generation_reader.lock().map_err(|_| {
                ContentStoreError::Io(io::Error::other("content generation reader mutex poisoned"))
            })?;
            if cached
                .as_ref()
                .is_none_or(|reader| !reader.matches(generation))
            {
                *cached = None;
                *cached = Some(
                    GenerationReader::open(&self.root, generation)
                        .map_err(|reason| ContentStoreError::InvalidFormat { id, reason })?,
                );
            }
            if let Some(record) = cached
                .as_mut()
                .expect("generation reader initialized")
                .read(id)?
            {
                return Ok(record);
            }
        }
        drop(catalog);
        // Legacy packed segments store raw content bytes, not envelopes.
        let value = self
            .catalog
            .lock()
            .map_err(|_| ContentStoreError::Io(io::Error::other("packed catalog mutex poisoned")))?
            .get_bounded(CONTENT_NAMESPACE, *id.as_bytes(), usize::MAX)
            .map_err(|error| content_catalog_error(id, error))?;
        if let Some((_kind, bytes)) = value {
            let header = crate::codec::EnvelopeHeader {
                codec: crate::codec::Codec::Raw,
                stored_length: bytes.len() as u64,
                logical_length: bytes.len() as u64,
            };
            return Ok((header, bytes));
        }
        // Fall back to loose content store (which uses envelope files).
        self.content.read_local_raw_envelope(id)
    }

    fn put_raw_envelopes(
        &self,
        envelopes: &[(ContentId, Vec<u8>)],
    ) -> Result<(), ContentStoreError> {
        // Delta envelopes are stored in the loose content directory. The read
        // path (get_bounded) resolves delta chains via get_raw_envelope which
        // checks both the packed catalog and loose files, so a delta whose base
        // lives in a packed segment resolves correctly.
        for (id, envelope) in envelopes {
            self.content.put_raw_envelope(*id, envelope.clone())?;
        }
        Ok(())
    }

    fn supports_raw_envelopes(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn traced_deferred_store_counts_storage_work_without_extending_its_lifetime() {
        let root = tempfile::tempdir().unwrap();
        let (store, trace) = PackedObjectStore::open_deferred_traced(root.path()).unwrap();
        let bytes = wheat_core::DirectoryNode::default().canonical_bytes();
        let id = ObjectId::of_canonical_bytes(&bytes);
        store.put_if_absent(id, &bytes).await.unwrap();
        store.get(id).await.unwrap();

        let snapshot = trace.snapshot().unwrap();
        assert_eq!(snapshot.object_put_attempts, 1);
        assert_eq!(snapshot.object_gets, 1);
        assert!(snapshot.catalog_gets >= 2);
        assert_eq!(snapshot.deferred_appends, 1);
        assert_eq!(snapshot.deferred_index_transactions, 1);
        assert_eq!(snapshot.locator_recounts, 0);

        let final_snapshot = store.close_with_trace().unwrap().unwrap();
        assert_eq!(final_snapshot.object_put_attempts, 1);
        assert_eq!(final_snapshot.bulk_finalizations, 1);
        assert_eq!(final_snapshot.locator_recounts, 0);
        assert!(trace.snapshot().is_none());
    }

    #[test]
    fn delta_across_packed_and_loose_resolves_through_chain() {
        let root = tempfile::tempdir().unwrap();
        let store = PackedObjectStore::open_deferred(root.path()).unwrap();
        let content = store.content_store();

        // Base content stored raw → goes to packed segment via put_many backend.
        let base = b"hello world this is the base content for delta testing purposes here";
        let base_id = content.put(base).unwrap();
        assert_eq!(content.get(base_id).unwrap(), base);

        // Build a chain up to depth 14 (one below MAX_DELTA_CHAIN_DEPTH).
        let mut prev_id = base_id;
        let mut versions: Vec<(wheat_core::ContentId, Vec<u8>)> = Vec::new();
        for i in 1..=14 {
            let data = format!(
                "hello world this is version {i:03} content for delta testing purposes here"
            );
            let data = data.into_bytes();
            let id = wheat_core::ContentId::of(&data);
            content.put_with_base(id, &data, prev_id).unwrap();
            let read_back = content.get(id).unwrap();
            assert_eq!(read_back, data, "depth-{i} delta read failed");
            versions.push((id, data));
            prev_id = id;
        }

        // Verify all versions are still readable.
        for (i, (id, expected)) in versions.iter().enumerate() {
            let read_back = content.get(*id).unwrap();
            assert_eq!(&read_back, expected, "re-read of depth-{} failed", i + 1);
        }
    }

    #[test]
    fn deferred_close_failure_is_returned_by_packed_store() {
        let root = tempfile::tempdir().unwrap();
        let store = PackedObjectStore::open_deferred(root.path()).unwrap();
        store.test_fail_close_before_finalize().unwrap();
        let error = store.close().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("test failure before deferred bulk finalization")
        );
        PackedObjectStore::open_deferred(root.path())
            .unwrap()
            .close()
            .unwrap();
    }
}
